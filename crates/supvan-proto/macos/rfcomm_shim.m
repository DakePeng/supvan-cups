// IOBluetooth RFCOMM for supvan-proto on macOS.
//
// macOS has no socket API for RFCOMM, and the /dev/cu.* port it publishes for
// a paired SPP device does not reliably bring the link up (macOS 26: the open
// returns at once and writes vanish). IOBluetoothDevice/IOBluetoothRFCOMMChannel
// work, but they are Objective-C and deliver their callbacks on the run loop
// of the thread that opened the channel. This shim owns one such thread and
// exposes a blocking byte-pipe C API that `rfcomm_macos.rs` wraps.
//
// Threading: every IOBluetooth call is performed on `SVBtThread` (which runs
// an NSRunLoop forever). Received bytes are appended under a mutex and a
// condition variable wakes readers. Reads and the open wait never block the
// Bluetooth thread.

#import <Foundation/Foundation.h>
#import <IOBluetooth/IOBluetooth.h>
#include <pthread.h>
#include <string.h>
#include <stdint.h>
#include <sys/time.h>

// ---------------------------------------------------------------------------
// The run-loop thread.

@interface SVBtThread : NSObject
+ (NSThread *)thread;
- (void)run:(id)unused;
@end

@implementation SVBtThread
+ (NSThread *)thread {
    static NSThread *t;
    static dispatch_once_t once;
    dispatch_once(&once, ^{
        SVBtThread *owner = [SVBtThread new];
        t = [[NSThread alloc] initWithTarget:owner selector:@selector(run:) object:nil];
        t.name = @"supvan-iobluetooth";
        [t start];
    });
    return t;
}
- (void)run:(id)unused {
    @autoreleasepool {
        // A port keeps the loop from returning when no sources are attached.
        [[NSRunLoop currentRunLoop] addPort:[NSMachPort port] forMode:NSDefaultRunLoopMode];
        while (1) {
            @autoreleasepool {
                [[NSRunLoop currentRunLoop] runMode:NSDefaultRunLoopMode
                                         beforeDate:[NSDate dateWithTimeIntervalSinceNow:1.0]];
            }
        }
    }
}
@end

// ---------------------------------------------------------------------------
// One link.

@interface SVLink : NSObject <IOBluetoothRFCOMMChannelDelegate> {
  @public
    IOBluetoothDevice *dev;
    IOBluetoothRFCOMMChannel *chan;
    NSMutableData *rx;
    pthread_mutex_t mu;
    pthread_cond_t cv;
    IOReturn openStatus;
    BOOL openDone;
    BOOL closed;
    BluetoothRFCOMMChannelID channelID;
    IOReturn lastWrite;
}
@end

@implementation SVLink
- (instancetype)init {
    if ((self = [super init])) {
        rx = [NSMutableData new];
        pthread_mutex_init(&mu, NULL);
        pthread_cond_init(&cv, NULL);
        openStatus = kIOReturnNotReady;
    }
    return self;
}
- (void)dealloc {
    pthread_mutex_destroy(&mu);
    pthread_cond_destroy(&cv);
}

// Runs on SVBtThread. The synchronous open brings the baseband link and the
// RFCOMM channel up before returning; the delegate's open-complete callback
// follows once this thread's run loop spins again, and `sv_rfcomm_open`
// waits for it (or for `isOpen`) before handing the link out — the first
// write right after a Sync open otherwise fails with kIOReturnNotOpen.
- (void)doOpen:(id)unused {
    IOBluetoothRFCOMMChannel *c = nil;
    IOReturn rc = [dev openRFCOMMChannelSync:&c withChannelID:channelID delegate:self];
    pthread_mutex_lock(&mu);
    chan = c;
    if (rc != kIOReturnSuccess) {
        openStatus = rc;
        openDone = YES;
        pthread_cond_broadcast(&cv);
    }
    pthread_mutex_unlock(&mu);
}

// Runs on SVBtThread: whether the channel reports itself open.
- (void)doCheckOpen:(id)unused {
    BOOL open = [chan isOpen];
    pthread_mutex_lock(&mu);
    if (open && !openDone) {
        openStatus = kIOReturnSuccess;
        openDone = YES;
        pthread_cond_broadcast(&cv);
    }
    pthread_mutex_unlock(&mu);
}

// Runs on SVBtThread. writeSync does not need this thread's run loop to
// spin, so blocking here is fine; incoming data queues until we return.
- (void)doWrite:(NSData *)data {
    IOReturn rc = kIOReturnSuccess;
    BluetoothRFCOMMMTU mtu = [chan getMTU];
    if (mtu == 0) mtu = 127;
    const uint8_t *p = data.bytes;
    NSUInteger left = data.length;
    while (left > 0 && rc == kIOReturnSuccess) {
        UInt16 n = (UInt16)(left < mtu ? left : mtu);
        rc = [chan writeSync:(void *)p length:n];
        p += n;
        left -= n;
    }
    lastWrite = rc;
}

// Runs on SVBtThread.
- (void)doClose:(id)unused {
    [chan setDelegate:nil];
    [chan closeChannel];
    // Drop the baseband link too: the printer treats a lingering ACL
    // connection as "still connected" and refuses the phone app.
    [dev closeConnection];
}

- (void)rfcommChannelOpenComplete:(IOBluetoothRFCOMMChannel *)c status:(IOReturn)status {
    pthread_mutex_lock(&mu);
    openStatus = status;
    openDone = YES;
    pthread_cond_broadcast(&cv);
    pthread_mutex_unlock(&mu);
}
- (void)rfcommChannelData:(IOBluetoothRFCOMMChannel *)c data:(void *)p length:(size_t)len {
    pthread_mutex_lock(&mu);
    [rx appendBytes:p length:len];
    pthread_cond_broadcast(&cv);
    pthread_mutex_unlock(&mu);
}
- (void)rfcommChannelClosed:(IOBluetoothRFCOMMChannel *)c {
    pthread_mutex_lock(&mu);
    closed = YES;
    pthread_cond_broadcast(&cv);
    pthread_mutex_unlock(&mu);
}
@end

static void sv_deadline(struct timespec *ts, int timeout_ms) {
    struct timeval now;
    gettimeofday(&now, NULL);
    int64_t ns = (int64_t)now.tv_usec * 1000 + (int64_t)timeout_ms * 1000000;
    ts->tv_sec = now.tv_sec + ns / 1000000000;
    ts->tv_nsec = ns % 1000000000;
}

static void sv_seterr(char *err, size_t errlen, NSString *msg) {
    if (err && errlen) {
        strncpy(err, msg.UTF8String, errlen - 1);
        err[errlen - 1] = 0;
    }
}

// ---------------------------------------------------------------------------
// C API.

/// Open RFCOMM `channel` on the device at `addr` ("AA:BB:CC:DD:EE:FF", any
/// separator IOBluetooth accepts). Blocks up to `timeout_ms`. Returns an
/// opaque handle, or NULL with `err` filled in.
void *sv_rfcomm_open(const char *addr, uint8_t channel, int timeout_ms, char *err, size_t errlen) {
    @autoreleasepool {
        IOBluetoothDevice *dev = [IOBluetoothDevice deviceWithAddressString:[NSString stringWithUTF8String:addr]];
        if (!dev) {
            sv_seterr(err, errlen, @"invalid Bluetooth address");
            return NULL;
        }
        SVLink *link = [SVLink new];
        link->dev = dev;
        link->channelID = channel;
        [link performSelector:@selector(doOpen:)
                     onThread:[SVBtThread thread]
                   withObject:nil
                waitUntilDone:YES];

        // Wait for open-complete; after a short grace, accept `isOpen` instead
        // (the callback has been seen to arrive late or not at all).
        struct timespec ts;
        sv_deadline(&ts, timeout_ms < 1000 ? timeout_ms : 1000);
        pthread_mutex_lock(&link->mu);
        int waited = 0;
        while (!link->openDone && waited == 0) {
            waited = pthread_cond_timedwait(&link->cv, &link->mu, &ts);
        }
        BOOL pending = !link->openDone && link->chan != nil;
        pthread_mutex_unlock(&link->mu);
        if (pending) {
            [link performSelector:@selector(doCheckOpen:)
                         onThread:[SVBtThread thread]
                       withObject:nil
                    waitUntilDone:YES];
            sv_deadline(&ts, timeout_ms);
            pthread_mutex_lock(&link->mu);
            waited = 0;
            while (!link->openDone && waited == 0) {
                waited = pthread_cond_timedwait(&link->cv, &link->mu, &ts);
            }
            pthread_mutex_unlock(&link->mu);
        }
        pthread_mutex_lock(&link->mu);
        BOOL ok = link->openDone && link->openStatus == kIOReturnSuccess && link->chan != nil;
        IOReturn status = link->openStatus;
        BOOL done = link->openDone;
        pthread_mutex_unlock(&link->mu);

        if (!ok) {
            [link performSelector:@selector(doClose:)
                         onThread:[SVBtThread thread]
                       withObject:nil
                    waitUntilDone:YES];
            sv_seterr(err, errlen,
                      done ? [NSString stringWithFormat:@"RFCOMM open failed: IOReturn 0x%x", status]
                           : @"RFCOMM open timed out (printer off, out of range, or busy with another host?)");
            return NULL;
        }
        return (void *)CFBridgingRetain(link);
    }
}

/// Write all of `buf`. Returns `len`, or -1 on error.
ssize_t sv_rfcomm_write(void *h, const uint8_t *buf, size_t len) {
    @autoreleasepool {
        SVLink *link = (__bridge SVLink *)h;
        pthread_mutex_lock(&link->mu);
        BOOL closed = link->closed;
        pthread_mutex_unlock(&link->mu);
        if (closed) return -1;
        [link performSelector:@selector(doWrite:)
                     onThread:[SVBtThread thread]
                   withObject:[NSData dataWithBytes:buf length:len]
                waitUntilDone:YES];
        return link->lastWrite == kIOReturnSuccess ? (ssize_t)len : -1;
    }
}

/// Read up to `len` bytes, waiting up to `timeout_ms` for the first byte.
/// Returns the count, 0 on timeout, -1 once the channel has closed.
ssize_t sv_rfcomm_read(void *h, uint8_t *buf, size_t len, int timeout_ms) {
    SVLink *link = (__bridge SVLink *)h;
    struct timespec ts;
    sv_deadline(&ts, timeout_ms < 0 ? 0 : timeout_ms);
    pthread_mutex_lock(&link->mu);
    int waited = 0;
    while (link->rx.length == 0 && !link->closed && waited == 0 && timeout_ms > 0) {
        waited = pthread_cond_timedwait(&link->cv, &link->mu, &ts);
    }
    ssize_t n;
    if (link->rx.length > 0) {
        n = (ssize_t)(link->rx.length < len ? link->rx.length : len);
        memcpy(buf, link->rx.bytes, (size_t)n);
        [link->rx replaceBytesInRange:NSMakeRange(0, (NSUInteger)n) withBytes:NULL length:0];
    } else {
        n = link->closed ? -1 : 0;
    }
    pthread_mutex_unlock(&link->mu);
    return n;
}

/// Close the channel and the baseband connection, free the handle.
void sv_rfcomm_close(void *h) {
    @autoreleasepool {
        SVLink *link = (SVLink *)CFBridgingRelease(h);
        [link performSelector:@selector(doClose:)
                     onThread:[SVBtThread thread]
                   withObject:nil
                waitUntilDone:YES];
    }
}

/// Call `cb` once per paired Classic-Bluetooth device with its address
/// ("AA:BB:CC:DD:EE:FF") and name. Returns the count.
int sv_bt_paired_devices(void (*cb)(const char *addr, const char *name, void *ctx), void *ctx) {
    @autoreleasepool {
        int n = 0;
        for (IOBluetoothDevice *d in [IOBluetoothDevice pairedDevices]) {
            NSString *addr = [[d.addressString stringByReplacingOccurrencesOfString:@"-" withString:@":"] uppercaseString];
            NSString *name = d.name ?: @"";
            cb(addr.UTF8String, name.UTF8String, ctx);
            n++;
        }
        return n;
    }
}

// ---------------------------------------------------------------------------
// Main-thread run loop.
//
// IOBluetooth's first use initialises a CoreBluetooth coordinator that waits
// on a callback delivered through the *main* dispatch queue. A process whose
// main thread is parked in a tokio runtime never drains that queue, and the
// call blocks forever. So the binaries hand their main thread to this loop
// and run everything else on other threads.

/// Run the main run loop until `sv_stop_main_loop` is called. Must be called
/// on the main thread.
void sv_run_main_loop(void) {
    @autoreleasepool {
        [[NSRunLoop mainRunLoop] addPort:[NSMachPort port] forMode:NSDefaultRunLoopMode];
    }
    CFRunLoopRun();
}

/// Stop a `sv_run_main_loop` from any thread.
void sv_stop_main_loop(void) {
    CFRunLoopRef main = CFRunLoopGetMain();
    CFRunLoopPerformBlock(main, kCFRunLoopDefaultMode, ^{ CFRunLoopStop(main); });
    CFRunLoopWakeUp(main);
}
