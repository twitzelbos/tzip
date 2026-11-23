//
//  fskit_install.m
//
//  Obj-C shim over SystemExtensions.framework for the FSKit extension
//  install / uninstall flow. Exposes two blocking C entry points called
//  from Rust (`fskit_install.rs`):
//
//    int tzip_sysext_activate(const char* bundle_id, char** err_out);
//    int tzip_sysext_deactivate(const char* bundle_id, char** err_out);
//
//  Return 0 on success, -1 on failure (in which case *err_out holds a
//  malloc'd NUL-terminated error message the caller must free()).
//
//  The design is deliberately boring: build the appropriate
//  OSSystemExtensionRequest, set a delegate backed by a
//  dispatch_semaphore, submit on a background queue (so the delegate
//  callbacks are not blocked by our main-thread wait), then wait with
//  a long timeout and surface the outcome.
//

#import <Foundation/Foundation.h>
#import <SystemExtensions/SystemExtensions.h>
#import <stdlib.h>
#import <string.h>

// Approval can take arbitrarily long (a human clicking in System
// Settings). 5 minutes is our outer bound before we give up.
#define TZIP_SYSEXT_TIMEOUT_SEC 300

typedef enum {
    TZIP_SYSEXT_PENDING = 0,
    TZIP_SYSEXT_OK = 1,
    TZIP_SYSEXT_FAIL = -1,
} TzipSysextStatus;

@interface TzipSysextDelegate : NSObject <OSSystemExtensionRequestDelegate>
@property (atomic, assign) TzipSysextStatus status;
@property (atomic, copy) NSString* errorMessage;
@property (atomic, assign) BOOL approvalAnnounced;
@property (atomic, strong) dispatch_semaphore_t sem;
@end

@implementation TzipSysextDelegate

- (OSSystemExtensionReplacementAction)request:(OSSystemExtensionRequest *)request
                  actionForReplacingExtension:(OSSystemExtensionProperties *)existing
                                withExtension:(OSSystemExtensionProperties *)ext
{
    // Always upgrade. The project ships a single-bundle-identifier
    // extension; any discovered install is ours and should be replaced.
    return OSSystemExtensionReplacementActionReplace;
}

- (void)requestNeedsUserApproval:(OSSystemExtensionRequest *)request
{
    if (!self.approvalAnnounced) {
        self.approvalAnnounced = YES;
        fprintf(stderr,
                "tzip: the extension needs one-time approval.\n"
                "      Open System Settings → General → Login Items & "
                "Extensions, scroll to File System Extensions, and approve "
                "'TzipFSExtension'. Waiting up to %d seconds...\n",
                TZIP_SYSEXT_TIMEOUT_SEC);
    }
}

- (void)request:(OSSystemExtensionRequest *)request
    didFinishWithResult:(OSSystemExtensionRequestResult)result
{
    if (result == OSSystemExtensionRequestCompleted) {
        self.status = TZIP_SYSEXT_OK;
    } else {
        // OSSystemExtensionRequestWillCompleteAfterReboot
        self.status = TZIP_SYSEXT_OK;
        fprintf(stderr,
                "tzip: extension will complete installation after reboot.\n");
    }
    dispatch_semaphore_signal(self.sem);
}

- (void)request:(OSSystemExtensionRequest *)request
    didFailWithError:(NSError *)error
{
    self.status = TZIP_SYSEXT_FAIL;
    self.errorMessage = [NSString stringWithFormat:@"%@ (code %ld)",
                         error.localizedDescription, (long)error.code];
    dispatch_semaphore_signal(self.sem);
}

@end

static char* copy_cstr(NSString* s)
{
    const char* utf8 = s.UTF8String ?: "unknown error";
    size_t len = strlen(utf8);
    char* out = (char*)malloc(len + 1);
    memcpy(out, utf8, len + 1);
    return out;
}

static int tzip_sysext_run(OSSystemExtensionRequest* req, char** err_out)
{
    @autoreleasepool {
        TzipSysextDelegate* delegate = [TzipSysextDelegate new];
        delegate.sem = dispatch_semaphore_create(0);
        req.delegate = delegate;

        [[OSSystemExtensionManager sharedManager] submitRequest:req];

        dispatch_time_t deadline = dispatch_time(
            DISPATCH_TIME_NOW, (int64_t)TZIP_SYSEXT_TIMEOUT_SEC * NSEC_PER_SEC);
        long waited = dispatch_semaphore_wait(delegate.sem, deadline);
        if (waited != 0) {
            if (err_out) {
                *err_out = copy_cstr(
                    @"timed out waiting for System Settings approval");
            }
            return -1;
        }
        if (delegate.status == TZIP_SYSEXT_OK) {
            return 0;
        }
        if (err_out) {
            NSString* msg = delegate.errorMessage ?: @"unknown error";
            *err_out = copy_cstr(msg);
        }
        return -1;
    }
}

int tzip_sysext_activate(const char* bundle_id, char** err_out)
{
    if (bundle_id == NULL) {
        if (err_out) {
            *err_out = copy_cstr(@"NULL bundle_id");
        }
        return -1;
    }
    @autoreleasepool {
        NSString* bid = [NSString stringWithUTF8String:bundle_id];
        dispatch_queue_t q = dispatch_queue_create(
            "io.qbio.tzip.sysext", DISPATCH_QUEUE_SERIAL);
        OSSystemExtensionRequest* req =
            [OSSystemExtensionRequest activationRequestForExtension:bid queue:q];
        return tzip_sysext_run(req, err_out);
    }
}

int tzip_sysext_deactivate(const char* bundle_id, char** err_out)
{
    if (bundle_id == NULL) {
        if (err_out) {
            *err_out = copy_cstr(@"NULL bundle_id");
        }
        return -1;
    }
    @autoreleasepool {
        NSString* bid = [NSString stringWithUTF8String:bundle_id];
        dispatch_queue_t q = dispatch_queue_create(
            "io.qbio.tzip.sysext", DISPATCH_QUEUE_SERIAL);
        OSSystemExtensionRequest* req =
            [OSSystemExtensionRequest deactivationRequestForExtension:bid queue:q];
        return tzip_sysext_run(req, err_out);
    }
}

void tzip_sysext_free_error(char* err)
{
    if (err != NULL) {
        free(err);
    }
}
