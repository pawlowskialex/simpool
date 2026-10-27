// simbridge: drives one booted simulator through Apple's private simulator frameworks.
//
//   simbridge <developer-dir> <udid>
//
// stdin:  one JSON command per line, {"id": 1, "op": "tap", ...}
// stdout: frames of [kind:u8][len:u32 big-endian][payload]
//         'R' JSON reply {"id": 1, "ok": true, ...} or {"id": 1, "ok": false, "error": "..."}
//         'F' JPEG screen frame (only while a stream is running)
// stderr: diagnostics
//
// Coordinates are in points (the units UIKit and the accessibility tree use), origin top-left.

#import <AppKit/AppKit.h>
#import <CoreImage/CoreImage.h>
#import <Foundation/Foundation.h>
#import <ImageIO/ImageIO.h>
#import <IOSurface/IOSurface.h>
#import <objc/message.h>
#import <objc/runtime.h>
#include <dlfcn.h>
#include <mach/mach_time.h>

#define MSG0(T, o, s) ((T(*)(id, SEL))objc_msgSend)(o, sel_registerName(s))

@interface NSObject (SimbridgePrivate)
+ (id)sharedInstance;
- (void)setBridgeTokenDelegate:(id)d;
- (void)setSupportsDelegateTokens:(BOOL)b;
- (void)setAccessibilityEnabled:(BOOL)b;
- (id)macPlatformElementFromTranslation:(id)t;
- (id)frontmostApplicationWithDisplayId:(unsigned)display bridgeDelegateToken:(NSString *)token;
- (id)objectAtPoint:(CGPoint)p displayId:(unsigned)display bridgeDelegateToken:(NSString *)token;
- (void)setBridgeDelegateToken:(NSString *)token;
- (id)translation;
@end

// SimulatorKit message builders.
typedef void *(*MouseMsgFn)(CGPoint *p0, CGPoint *p1, uint32_t target, NSEventType type, CGSize size, uint32_t edge);
typedef void *(*KeyMsgFn)(uint32_t usage, uint32_t direction);
typedef void *(*ButtonMsgFn)(uint32_t source, uint32_t direction, uint32_t target);
static MouseMsgFn gMouseMsg;
static KeyMsgFn gKeyMsg;
static ButtonMsgFn gButtonMsg;

enum { DirDown = 1, DirUp = 2 };
static const uint32_t TargetScreen = 0x32, TargetHardware = 0x33;

static id gDevice, gHID;
static CGSize gPoints, gPixels;
static float gScale;
static dispatch_queue_t gHIDQueue, gAXQueue, gStreamQueue;
static NSLock *gOut;
static CIContext *gCI;

#pragma mark - output

static void emit(char kind, NSData *payload) {
  uint8_t hdr[5] = {(uint8_t)kind};
  uint32_t n = htonl((uint32_t)payload.length);
  memcpy(hdr + 1, &n, 4);
  [gOut lock];
  fwrite(hdr, 1, 5, stdout);
  fwrite(payload.bytes, 1, payload.length, stdout);
  fflush(stdout);
  [gOut unlock];
}

static void reply(id cid, NSDictionary *body, NSString *error) {
  NSMutableDictionary *d = [NSMutableDictionary dictionaryWithDictionary:body ?: @{}];
  d[@"id"] = cid ?: [NSNull null];
  d[@"ok"] = error ? @NO : @YES;
  if (error) d[@"error"] = error;
  NSError *e = nil;
  NSData *json = [NSJSONSerialization dataWithJSONObject:d options:0 error:&e];
  if (!json) json = [NSJSONSerialization dataWithJSONObject:@{@"id": cid ?: [NSNull null], @"ok": @NO, @"error": e.localizedDescription} options:0 error:nil];
  emit('R', json);
}

#pragma mark - HID

static NSString *sendHID(void *msg) {
  if (!msg) return nil;  // SimulatorKit drops moves that come too close together
  __block NSString *err = nil;
  dispatch_semaphore_t done = dispatch_semaphore_create(0);
  ((void (*)(id, SEL, void *, BOOL, id, id))objc_msgSend)(
      gHID, sel_registerName("sendWithMessage:freeWhenDone:completionQueue:completion:"), msg, YES, gHIDQueue, ^(NSError *e) {
        if (e) err = e.localizedDescription ?: e.description;
        dispatch_semaphore_signal(done);
      });
  dispatch_semaphore_wait(done, DISPATCH_TIME_FOREVER);
  return err;
}

static NSString *touch(NSEventType type, CGPoint p, const CGPoint *p2) {
  CGPoint a = p, b = p2 ? *p2 : CGPointZero;
  return sendHID(gMouseMsg(&a, p2 ? &b : NULL, TargetScreen, type, gPoints, 0));
}

static void sleepSec(double s) {
  if (s > 0) usleep((useconds_t)(s * 1e6));
}

static CGPoint lerp(CGPoint a, CGPoint b, double t) {
  return CGPointMake(a.x + (b.x - a.x) * t, a.y + (b.y - a.y) * t);
}

// Moves finger(s) along straight lines from a to b over `duration`, down and up
// included. `hold` pauses after touch-down (press-then-drag); `settle` holds the
// finger still at the end before lifting. A nonzero `settle` drops the finger's
// velocity to ~0 at lift-off, so UIKit scrolls exactly the dragged distance with
// no momentum fling; `settle` 0 keeps the finger moving into the lift, which
// UIKit reads as a flick and carries on with deceleration.
static NSString *gesture(CGPoint a0, CGPoint a1, const CGPoint *b0, const CGPoint *b1, double duration, double hold, double settle) {
  NSString *err = touch(NSEventTypeLeftMouseDown, a0, b0);
  if (err) return err;
  sleepSec(hold);
  CGPoint pb = b1 ? *b1 : CGPointZero;
  // With settle>0 the finger decelerates into the lift: the move eases out over
  // duration+settle so its last samples are tiny, distinct steps, which UIKit
  // reads as ~0 velocity and so adds no momentum fling (content stops exactly at
  // the dragged distance). Holding perfectly still can't do this: SimulatorKit
  // drops repeated identical points, so UIKit would keep the pre-stop velocity.
  // settle==0 keeps constant velocity into the lift: a flick that carries on.
  double total = duration + settle;
  int steps = MAX(1, (int)(total / 0.018));
  for (int i = 1; i <= steps; i++) {
    double t = (double)i / steps;
    double e = settle > 0 ? 1.0 - pow(1.0 - t, 3.0) : t;  // cubic ease-out
    CGPoint pa = lerp(a0, a1, e), p2 = b0 ? lerp(*b0, *b1, e) : CGPointZero;
    sleepSec(total / steps);
    if ((err = touch(NSEventTypeLeftMouseDragged, pa, b0 ? &p2 : NULL))) return err;
  }
  return touch(NSEventTypeLeftMouseUp, a1, b1 ? &pb : NULL);
}

static NSString *key(uint32_t usage, uint32_t dir) { return sendHID(gKeyMsg(usage, dir)); }

static NSString *keyPress(uint32_t usage, NSArray<NSNumber *> *mods, double hold) {
  NSString *err = nil;
  for (NSNumber *m in mods) if ((err = key(m.unsignedIntValue, DirDown))) return err;
  if ((err = key(usage, DirDown))) return err;
  sleepSec(hold);
  err = key(usage, DirUp);
  for (NSNumber *m in mods.reverseObjectEnumerator) key(m.unsignedIntValue, DirUp);
  return err;
}

// US layout: character -> HID usage (page 7), plus whether shift is needed.
static BOOL usageForChar(unichar c, uint32_t *usage, BOOL *shift) {
  static NSDictionary<NSString *, NSArray *> *punct;
  static dispatch_once_t once;
  dispatch_once(&once, ^{
    punct = @{
      @"\n": @[@40, @NO], @"\r": @[@40, @NO], @"\t": @[@43, @NO], @" ": @[@44, @NO],
      @"-": @[@45, @NO], @"=": @[@46, @NO], @"[": @[@47, @NO], @"]": @[@48, @NO], @"\\": @[@49, @NO],
      @";": @[@51, @NO], @"'": @[@52, @NO], @"`": @[@53, @NO], @",": @[@54, @NO], @".": @[@55, @NO], @"/": @[@56, @NO],
      @"_": @[@45, @YES], @"+": @[@46, @YES], @"{": @[@47, @YES], @"}": @[@48, @YES], @"|": @[@49, @YES],
      @":": @[@51, @YES], @"\"": @[@52, @YES], @"~": @[@53, @YES], @"<": @[@54, @YES], @">": @[@55, @YES], @"?": @[@56, @YES],
      @"!": @[@30, @YES], @"@": @[@31, @YES], @"#": @[@32, @YES], @"$": @[@33, @YES], @"%": @[@34, @YES],
      @"^": @[@35, @YES], @"&": @[@36, @YES], @"*": @[@37, @YES], @"(": @[@38, @YES], @")": @[@39, @YES],
    };
  });
  if (c >= 'a' && c <= 'z') { *usage = 4 + (c - 'a'); *shift = NO; return YES; }
  if (c >= 'A' && c <= 'Z') { *usage = 4 + (c - 'A'); *shift = YES; return YES; }
  if (c >= '1' && c <= '9') { *usage = 30 + (c - '1'); *shift = NO; return YES; }
  if (c == '0') { *usage = 39; *shift = NO; return YES; }
  NSArray *p = punct[[NSString stringWithCharacters:&c length:1]];
  if (!p) return NO;
  *usage = [p[0] unsignedIntValue];
  *shift = [p[1] boolValue];
  return YES;
}

static NSDictionary<NSString *, NSNumber *> *namedKeys(void) {
  return @{
    @"enter": @40, @"return": @40, @"escape": @41, @"esc": @41, @"backspace": @42, @"delete": @42, @"tab": @43, @"space": @44,
    @"forward_delete": @76, @"home": @74, @"end": @77, @"page_up": @75, @"page_down": @78,
    @"right": @79, @"left": @80, @"down": @81, @"up": @82,
    @"f1": @58, @"f2": @59, @"f3": @60, @"f4": @61, @"f5": @62, @"f6": @63, @"f7": @64, @"f8": @65, @"f9": @66, @"f10": @67, @"f11": @68, @"f12": @69,
    @"ctrl": @224, @"control": @224, @"shift": @225, @"alt": @226, @"option": @226, @"cmd": @227, @"command": @227,
  };
}

static NSDictionary<NSString *, NSNumber *> *buttonSources(void) {
  return @{@"home": @0x0, @"lock": @0x1, @"power": @0x1, @"side": @0xbb8, @"siri": @0x400002, @"apple_pay": @0x1f4};
}

#pragma mark - accessibility

@interface AXBridge : NSObject
@end
@implementation AXBridge
- (id)accessibilityTranslationDelegateBridgeCallbackWithToken:(NSString *)token {
  return ^id(id request) {
    __block id resp = nil;
    dispatch_semaphore_t done = dispatch_semaphore_create(0);
    ((void (*)(id, SEL, id, id, id))objc_msgSend)(
        gDevice, sel_registerName("sendAccessibilityRequestAsync:completionQueue:completionHandler:"), request, gAXQueue, ^(id r) {
          resp = r;
          dispatch_semaphore_signal(done);
        });
    if (dispatch_semaphore_wait(done, dispatch_time(DISPATCH_TIME_NOW, 10 * NSEC_PER_SEC))) return nil;
    return resp;
  };
}
- (CGRect)accessibilityTranslationConvertPlatformFrameToSystem:(CGRect)rect withToken:(NSString *)token { return rect; }
- (id)accessibilityTranslationRootParentWithToken:(NSString *)token { return nil; }
@end

static id gTranslator;
static AXBridge *gAXBridge;

static id str(id v) {
  if (!v || v == [NSNull null]) return nil;
  if ([v isKindOfClass:NSString.class]) return [v length] ? v : nil;
  if ([v isKindOfClass:NSAttributedString.class]) return [v string];
  if ([v isKindOfClass:NSNumber.class]) return [v stringValue];
  return [v description];
}

static id safe(id (^get)(void)) {
  @try { return get(); } @catch (NSException *e) { return nil; }
}

static NSDictionary *elementJSON(id el, int depth, int maxDepth, int *budget) {
  NSMutableDictionary *d = [NSMutableDictionary dictionary];
  NSRect f = [el accessibilityFrame];
  d[@"frame"] = @{@"x": @(f.origin.x), @"y": @(f.origin.y), @"width": @(f.size.width), @"height": @(f.size.height)};
  NSDictionary *attrs = @{
    @"role": safe(^{ return str([el accessibilityRole]); }) ?: [NSNull null],
    @"subrole": safe(^{ return str([el accessibilitySubrole]); }) ?: [NSNull null],
    @"label": safe(^{ return str([el accessibilityLabel]); }) ?: [NSNull null],
    @"value": safe(^{ return str([el accessibilityValue]); }) ?: [NSNull null],
    @"identifier": safe(^{ return str([el accessibilityIdentifier]); }) ?: [NSNull null],
    @"title": safe(^{ return str([el accessibilityTitle]); }) ?: [NSNull null],
    @"help": safe(^{ return str([el accessibilityHelp]); }) ?: [NSNull null],
    @"placeholder": safe(^{ return str([el accessibilityPlaceholderValue]); }) ?: [NSNull null],
  };
  for (NSString *k in attrs) if (attrs[k] != [NSNull null]) d[k] = attrs[k];
  d[@"enabled"] = @([el isAccessibilityEnabled]);
  if ([el isAccessibilityFocused]) d[@"focused"] = @YES;
  if ([el isAccessibilitySelected]) d[@"selected"] = @YES;
  if (depth < maxDepth && --*budget > 0) {
    NSArray *kids = safe(^{ return [el accessibilityChildren]; });
    if (kids.count) {
      NSMutableArray *out = [NSMutableArray arrayWithCapacity:kids.count];
      for (id c in kids) {
        if (*budget <= 0) break;
        [out addObject:elementJSON(c, depth + 1, maxDepth, budget)];
      }
      d[@"children"] = out;
    }
  }
  return d;
}

static id platformElement(id translation, NSString *token) {
  if (!translation) return nil;
  [translation setBridgeDelegateToken:token];
  id el = [gTranslator macPlatformElementFromTranslation:translation];
  [[el translation] setBridgeDelegateToken:token];
  return el;
}

#pragma mark - screen

static IOSurfaceRef gSurface;  // retained
static id gScreenPort;

static id mainScreenDescriptor(void) {
  if (gScreenPort) return gScreenPort;
  id io = [gDevice valueForKey:@"io"];
  for (id port in MSG0(id, io, "ioPorts")) {
    id d = MSG0(id, port, "descriptor");
    if (![d conformsToProtocol:objc_getProtocol("SimDisplayIOSurfaceRenderable")]) continue;
    id st = MSG0(id, d, "state");
    if ([st respondsToSelector:sel_registerName("displayClass")] && MSG0(unsigned short, st, "displayClass") != 0) continue;
    gScreenPort = d;
    ((void (*)(id, SEL, id, id))objc_msgSend)(d, sel_registerName("registerCallbackWithUUID:ioSurfacesChangeCallback:"), [NSUUID UUID], ^(id s) {
      dispatch_async(gStreamQueue, ^{
        if (gSurface) CFRelease(gSurface);
        gSurface = NULL;
      });
    });
    return d;
  }
  return nil;
}

// Call on gStreamQueue.
static IOSurfaceRef currentSurface(void) {
  if (gSurface) return gSurface;
  id d = mainScreenDescriptor();
  id s = d ? MSG0(id, d, "framebufferSurface") : nil;
  if (s) gSurface = (IOSurfaceRef)CFRetain((__bridge CFTypeRef)s);
  return gSurface;
}

static NSData *encode(IOSurfaceRef s, NSString *format, double quality, double scale) {
  CIImage *img = [CIImage imageWithIOSurface:s];
  if (scale > 0 && scale < 1) img = [img imageByApplyingTransform:CGAffineTransformMakeScale(scale, scale) highQualityDownsample:YES];
  CGColorSpaceRef cs = CGColorSpaceCreateWithName(kCGColorSpaceSRGB);
  NSData *out;
  if ([format isEqualToString:@"png"]) {
    out = [gCI PNGRepresentationOfImage:img format:kCIFormatBGRA8 colorSpace:cs options:@{}];
  } else {
    out = [gCI JPEGRepresentationOfImage:img colorSpace:cs options:@{(id)kCGImageDestinationLossyCompressionQuality: @(quality)}];
  }
  CGColorSpaceRelease(cs);
  return out;
}

static dispatch_source_t gStreamTimer;
static uint32_t gLastSeed;
static uint64_t gLastFrameAt;

static void stopStream(void) {
  if (gStreamTimer) dispatch_source_cancel(gStreamTimer);
  gStreamTimer = nil;
}

static void startStream(double fps, double quality, double scale) {
  stopStream();
  gLastSeed = 0;
  gLastFrameAt = 0;
  dispatch_source_t t = dispatch_source_create(DISPATCH_SOURCE_TYPE_TIMER, 0, 0, gStreamQueue);
  uint64_t interval = (uint64_t)(NSEC_PER_SEC / MAX(1.0, MIN(fps, 60.0)));
  dispatch_source_set_timer(t, DISPATCH_TIME_NOW, interval, interval / 10);
  dispatch_source_set_event_handler(t, ^{
    @autoreleasepool {
      IOSurfaceRef s = currentSurface();
      if (!s) return;
      uint32_t seed = IOSurfaceGetSeed(s);
      uint64_t now = clock_gettime_nsec_np(CLOCK_MONOTONIC);
      // Only send changed frames, plus one a second so late joiners and idle screens still get a picture.
      if (seed == gLastSeed && now - gLastFrameAt < NSEC_PER_SEC) return;
      gLastSeed = seed;
      gLastFrameAt = now;
      NSData *jpg = encode(s, @"jpeg", quality, scale);
      if (jpg) emit('F', jpg);
    }
  });
  gStreamTimer = t;
  dispatch_resume(t);
}

#pragma mark - commands

static double num(NSDictionary *c, NSString *k, double dflt) {
  id v = c[k];
  return [v isKindOfClass:NSNumber.class] ? [v doubleValue] : dflt;
}

static BOOL point(NSDictionary *c, NSString *xk, NSString *yk, CGPoint *out) {
  if (![c[xk] isKindOfClass:NSNumber.class] || ![c[yk] isKindOfClass:NSNumber.class]) return NO;
  *out = CGPointMake([c[xk] doubleValue], [c[yk] doubleValue]);
  return YES;
}

static NSArray<NSNumber *> *modifiers(NSDictionary *c, NSString **err) {
  NSMutableArray *mods = [NSMutableArray array];
  for (NSString *m in c[@"modifiers"] ?: @[]) {
    NSNumber *u = namedKeys()[m.lowercaseString];
    if (!u || u.intValue < 224) { *err = [NSString stringWithFormat:@"unknown modifier %@", m]; return nil; }
    [mods addObject:u];
  }
  return mods;
}

static NSString *typeString(NSString *text, double interval) {
  NSString *err = nil;
  for (NSUInteger i = 0; i < text.length; i++) {
    uint32_t usage;
    BOOL shift;
    unichar c = [text characterAtIndex:i];
    if (!usageForChar(c, &usage, &shift))
      return [NSString stringWithFormat:@"can't type %@ with a US hardware keyboard; use the text action, which falls back to the pasteboard", [NSString stringWithCharacters:&c length:1]];
    if ((err = keyPress(usage, shift ? @[@225] : @[], 0.005))) return err;
    sleepSec(interval);
  }
  return nil;
}

static void handle(NSDictionary *c) {
  id cid = c[@"id"];
  NSString *op = c[@"op"];
  NSString *err = nil;
  CGPoint a, b, a2, b2;

  if ([op isEqualToString:@"info"]) {
    id dt = [gDevice valueForKey:@"deviceType"];
    id rt = [gDevice valueForKey:@"runtime"];
    reply(cid, @{
      @"name": [gDevice valueForKey:@"name"] ?: @"",
      @"device_type": [dt valueForKey:@"name"] ?: @"",
      @"runtime": [rt valueForKey:@"name"] ?: @"",
      @"runtime_build": [rt valueForKey:@"buildVersionString"] ?: @"",
      @"state": [gDevice valueForKey:@"stateString"] ?: @"",
      @"points": @{@"width": @(gPoints.width), @"height": @(gPoints.height)},
      @"pixels": @{@"width": @(gPixels.width), @"height": @(gPixels.height)},
      @"scale": @(gScale),
    }, nil);
  } else if ([op isEqualToString:@"touch"]) {
    // Raw touch primitive: phase down|move|up, optional second finger (x2, y2).
    if (!point(c, @"x", @"y", &a)) return reply(cid, nil, @"touch needs x and y");
    BOOL two = point(c, @"x2", @"y2", &b);
    NSString *phase = c[@"phase"];
    NSEventType t = [phase isEqualToString:@"down"] ? NSEventTypeLeftMouseDown : [phase isEqualToString:@"up"] ? NSEventTypeLeftMouseUp : [phase isEqualToString:@"move"] ? NSEventTypeLeftMouseDragged : 0;
    if (!t) return reply(cid, nil, @"phase must be down, move or up");
    reply(cid, nil, touch(t, a, two ? &b : NULL));
  } else if ([op isEqualToString:@"tap"]) {
    if (!point(c, @"x", @"y", &a)) return reply(cid, nil, @"tap needs x and y");
    int count = MAX(1, (int)num(c, @"count", 1));
    for (int i = 0; i < count && !err; i++) {
      if (i) sleepSec(0.08);
      if (!(err = touch(NSEventTypeLeftMouseDown, a, NULL))) {
        sleepSec(num(c, @"duration", 0.05));
        err = touch(NSEventTypeLeftMouseUp, a, NULL);
      }
    }
    reply(cid, nil, err);
  } else if ([op isEqualToString:@"swipe"]) {
    if (!point(c, @"x", @"y", &a) || !point(c, @"to_x", @"to_y", &b)) return reply(cid, nil, @"swipe needs x, y, to_x, to_y");
    reply(cid, nil, gesture(a, b, NULL, NULL, num(c, @"duration", 0.3), num(c, @"hold", 0), num(c, @"settle", 0)));
  } else if ([op isEqualToString:@"pan"]) {
    // Two fingers moving together from (x,y) by (dx,dy); `gap` apart (default 40pt).
    if (!point(c, @"x", @"y", &a)) return reply(cid, nil, @"pan needs x and y");
    double dx = num(c, @"dx", 0), dy = num(c, @"dy", 0), gap = num(c, @"gap", 40) / 2;
    a2 = CGPointMake(a.x - gap, a.y); b2 = CGPointMake(a.x + gap, a.y);
    CGPoint a3 = CGPointMake(a2.x + dx, a2.y + dy), b3 = CGPointMake(b2.x + dx, b2.y + dy);
    reply(cid, nil, gesture(a2, a3, &b2, &b3, num(c, @"duration", 0.3), 0, num(c, @"settle", 0)));
  } else if ([op isEqualToString:@"drag"]) {
    // Polyline drag with one finger: points [[x,y], ...], each segment takes duration/segments.
    NSArray *pts = c[@"points"];
    if (pts.count < 2) return reply(cid, nil, @"drag needs at least 2 points");
    double seg = num(c, @"duration", 0.5) / (pts.count - 1);
    a = CGPointMake([pts[0][0] doubleValue], [pts[0][1] doubleValue]);
    err = touch(NSEventTypeLeftMouseDown, a, NULL);
    sleepSec(num(c, @"hold", 0));
    for (NSUInteger i = 1; i < pts.count && !err; i++) {
      b = CGPointMake([pts[i][0] doubleValue], [pts[i][1] doubleValue]);
      int steps = MAX(1, (int)(seg / 0.018));
      for (int s = 1; s <= steps && !err; s++) {
        sleepSec(seg / steps);
        err = touch(NSEventTypeLeftMouseDragged, lerp(a, b, (double)s / steps), NULL);
      }
      a = b;
    }
    if (!err) err = touch(NSEventTypeLeftMouseUp, a, NULL);
    reply(cid, nil, err);
  } else if ([op isEqualToString:@"pinch"]) {
    // Two fingers move symmetrically around (x, y) from `from` to `to` apart; angle in degrees.
    if (!point(c, @"x", @"y", &a)) return reply(cid, nil, @"pinch needs x and y");
    double from = num(c, @"from", 100) / 2, to = num(c, @"to", 200) / 2, ang = num(c, @"angle", 0) * M_PI / 180;
    double dx = cos(ang), dy = sin(ang);
    a2 = CGPointMake(a.x - dx * from, a.y - dy * from);
    b2 = CGPointMake(a.x + dx * from, a.y + dy * from);
    CGPoint a3 = CGPointMake(a.x - dx * to, a.y - dy * to), b3 = CGPointMake(a.x + dx * to, a.y + dy * to);
    reply(cid, nil, gesture(a2, a3, &b2, &b3, num(c, @"duration", 0.5), 0, num(c, @"settle", 0)));
  } else if ([op isEqualToString:@"key"]) {
    NSArray *mods = modifiers(c, &err);
    if (err) return reply(cid, nil, err);
    uint32_t usage = 0;
    if ([c[@"key"] isKindOfClass:NSNumber.class]) usage = [c[@"key"] unsignedIntValue];
    else if ([c[@"key"] isKindOfClass:NSString.class]) {
      NSString *k = c[@"key"];
      NSNumber *n = namedKeys()[k.lowercaseString];
      BOOL shift = NO;
      if (n) usage = n.unsignedIntValue;
      else if (k.length == 1 && usageForChar([k characterAtIndex:0], &usage, &shift)) {
        if (shift) mods = [mods arrayByAddingObject:@225];
      } else return reply(cid, nil, [NSString stringWithFormat:@"unknown key %@", k]);
    } else return reply(cid, nil, @"key needs key (name, character or HID usage)");
    NSString *dir = c[@"direction"];
    if ([dir isEqualToString:@"down"]) err = key(usage, DirDown);
    else if ([dir isEqualToString:@"up"]) err = key(usage, DirUp);
    else err = keyPress(usage, mods, num(c, @"duration", 0.05));
    reply(cid, nil, err);
  } else if ([op isEqualToString:@"text"]) {
    reply(cid, nil, typeString(c[@"text"] ?: @"", num(c, @"interval", 0.01)));
  } else if ([op isEqualToString:@"button"]) {
    NSNumber *src = buttonSources()[c[@"button"]];
    if (!src) return reply(cid, nil, [NSString stringWithFormat:@"unknown button %@ (have %@)", c[@"button"], [buttonSources().allKeys componentsJoinedByString:@", "]]);
    if (!(err = sendHID(gButtonMsg(src.unsignedIntValue, DirDown, TargetHardware)))) {
      sleepSec(num(c, @"duration", 0.1));
      err = sendHID(gButtonMsg(src.unsignedIntValue, DirUp, TargetHardware));
    }
    reply(cid, nil, err);
  } else if ([op isEqualToString:@"ax_tree"] || [op isEqualToString:@"ax_at"]) {
    NSString *token = [NSUUID UUID].UUIDString;
    id tr;
    if ([op isEqualToString:@"ax_at"]) {
      if (!point(c, @"x", @"y", &a)) return reply(cid, nil, @"ax_at needs x and y");
      tr = [gTranslator objectAtPoint:a displayId:0 bridgeDelegateToken:token];
    } else {
      tr = [gTranslator frontmostApplicationWithDisplayId:0 bridgeDelegateToken:token];
    }
    id el = platformElement(tr, token);
    if (!el) return reply(cid, nil, @"no accessibility element (is the simulator still booting?)");
    int budget = (int)num(c, @"max_elements", 5000);
    NSMutableDictionary *root = [elementJSON(el, 0, (int)num(c, @"max_depth", 64), &budget) mutableCopy];
    pid_t pid = [[[el translation] valueForKey:@"pid"] intValue];
    if (pid) root[@"pid"] = @(pid);
    reply(cid, @{@"tree": root, @"truncated": budget <= 0 ? @YES : @NO}, nil);
  } else if ([op isEqualToString:@"screenshot"]) {
    __block NSData *img = nil;
    NSString *fmt = c[@"format"] ?: @"png";
    double q = num(c, @"quality", 0.8), s = num(c, @"scale", 1);
    dispatch_sync(gStreamQueue, ^{
      IOSurfaceRef surf = currentSurface();
      if (surf) img = encode(surf, fmt, q, s);
    });
    if (!img) return reply(cid, nil, @"no framebuffer (is the simulator booted?)");
    reply(cid, @{@"format": fmt, @"data": [img base64EncodedStringWithOptions:0]}, nil);
  } else if ([op isEqualToString:@"stream_start"]) {
    double fps = num(c, @"fps", 15), q = num(c, @"quality", 0.6), s = num(c, @"scale", 0.5);
    dispatch_sync(gStreamQueue, ^{ startStream(fps, q, s); });
    reply(cid, nil, nil);
  } else if ([op isEqualToString:@"stream_stop"]) {
    dispatch_sync(gStreamQueue, ^{ stopStream(); });
    reply(cid, nil, nil);
  } else if ([op isEqualToString:@"ping"]) {
    reply(cid, nil, nil);
  } else {
    reply(cid, nil, [NSString stringWithFormat:@"unknown op %@", op]);
  }
}

#pragma mark - setup

static void fail(NSString *msg) {
  fprintf(stderr, "simbridge: %s\n", msg.UTF8String);
  exit(1);
}

int main(int argc, char **argv) {
  @autoreleasepool {
    if (argc != 3) fail(@"usage: simbridge <developer-dir> <udid>");
    signal(SIGPIPE, SIG_IGN);
    NSString *devDir = @(argv[1]);
    if (!dlopen("/Library/Developer/PrivateFrameworks/CoreSimulator.framework/CoreSimulator", RTLD_NOW)) fail(@(dlerror()));
    void *sk = dlopen([devDir stringByAppendingPathComponent:@"Library/PrivateFrameworks/SimulatorKit.framework/SimulatorKit"].UTF8String, RTLD_NOW);
    if (!sk) fail(@(dlerror()));
    if (!dlopen("/System/Library/PrivateFrameworks/AccessibilityPlatformTranslation.framework/AccessibilityPlatformTranslation", RTLD_NOW)) fail(@(dlerror()));
    gMouseMsg = (MouseMsgFn)dlsym(sk, "IndigoHIDMessageForMouseNSEvent");
    gKeyMsg = (KeyMsgFn)dlsym(sk, "IndigoHIDMessageForKeyboardArbitrary");
    gButtonMsg = (ButtonMsgFn)dlsym(sk, "IndigoHIDMessageForButton");
    if (!gMouseMsg || !gKeyMsg || !gButtonMsg) fail(@"SimulatorKit is missing the Indigo HID functions; unsupported Xcode");

    NSError *err = nil;
    id ctx = ((id (*)(id, SEL, id, NSError **))objc_msgSend)(objc_getClass("SimServiceContext"), sel_registerName("sharedServiceContextForDeveloperDir:error:"), devDir, &err);
    id set = ctx ? ((id (*)(id, SEL, NSError **))objc_msgSend)(ctx, sel_registerName("defaultDeviceSetWithError:"), &err) : nil;
    if (!set) fail([NSString stringWithFormat:@"CoreSimulator: %@", err]);
    NSUUID *udid = [[NSUUID alloc] initWithUUIDString:@(argv[2])];
    gDevice = udid ? [set valueForKey:@"devicesByUDID"][udid] : nil;
    if (!gDevice) fail([NSString stringWithFormat:@"no simulator %s", argv[2]]);

    id dt = [gDevice valueForKey:@"deviceType"];
    gPixels = MSG0(CGSize, dt, "mainScreenSize");
    gScale = MSG0(float, dt, "mainScreenScale");
    gPoints = CGSizeMake(gPixels.width / gScale, gPixels.height / gScale);

    gHID = ((id (*)(id, SEL, id, NSError **))objc_msgSend)([objc_getClass("SimulatorKit.SimDeviceLegacyHIDClient") alloc], sel_registerName("initWithDevice:error:"), gDevice, &err);
    if (!gHID) fail([NSString stringWithFormat:@"HID client: %@", err]);

    gHIDQueue = dispatch_queue_create("simbridge.hid", DISPATCH_QUEUE_SERIAL);
    gAXQueue = dispatch_queue_create("simbridge.ax", DISPATCH_QUEUE_SERIAL);
    gStreamQueue = dispatch_queue_create("simbridge.stream", DISPATCH_QUEUE_SERIAL);
    gOut = [NSLock new];
    gCI = [CIContext contextWithOptions:@{kCIContextCacheIntermediates: @NO}];

    gTranslator = [objc_getClass("AXPTranslator") sharedInstance];
    gAXBridge = [AXBridge new];
    [gTranslator setBridgeTokenDelegate:gAXBridge];
    [gTranslator setSupportsDelegateTokens:YES];
    [gTranslator setAccessibilityEnabled:YES];

    // Commands run one at a time, in order, so gestures never interleave.
    dispatch_queue_t cmdQueue = dispatch_queue_create("simbridge.cmd", DISPATCH_QUEUE_SERIAL);
    [NSThread detachNewThreadWithBlock:^{
      char *line = NULL;
      size_t cap = 0;
      while (getline(&line, &cap, stdin) > 0) {
        NSData *data = [NSData dataWithBytes:line length:strlen(line)];
        dispatch_async(cmdQueue, ^{
          @autoreleasepool {
            NSError *e = nil;
            id cmd = [NSJSONSerialization JSONObjectWithData:data options:0 error:&e];
            if (![cmd isKindOfClass:NSDictionary.class]) return reply(nil, nil, [NSString stringWithFormat:@"bad command: %@", e.localizedDescription]);
            @try {
              handle(cmd);
            } @catch (NSException *ex) {
              reply(cmd[@"id"], nil, [NSString stringWithFormat:@"%@: %@", ex.name, ex.reason]);
            }
          }
        });
      }
      exit(0);  // stdin closed: the server went away
    }];
    reply(@0, @{@"ready": @YES}, nil);
    dispatch_main();
  }
}
