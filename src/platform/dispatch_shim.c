// Objective-C-blocks shim around dispatch_io_read.
//
// dispatch_io_read uses ^-blocks for its completion handler, which are not
// accessible from Rust FFI. This shim provides a synchronous read routine
// backed by dispatch_io_read + a semaphore, callable from plain Rust.
//
// Compiled by build.rs on macOS only.

#include <dispatch/dispatch.h>
#include <errno.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>
#include <unistd.h>

// Read up to `length` bytes from `fd` starting at offset 0.
//
// The channel takes ownership of `fd` and closes it via the cleanup handler.
// Bytes are copied into `out_buf`. On success returns 0 and writes the
// actual byte count into `*out_written`. On error returns errno-style code.
int tzip_dispatch_read_all_sync(
    int fd,
    size_t length,
    uint8_t *out_buf,
    size_t *out_written
) {
    if (length == 0) {
        *out_written = 0;
        close(fd);
        return 0;
    }

    dispatch_queue_t queue = dispatch_get_global_queue(QOS_CLASS_USER_INITIATED, 0);
    dispatch_semaphore_t sem = dispatch_semaphore_create(0);
    if (sem == NULL) {
        close(fd);
        return ENOMEM;
    }

    __block size_t offset = 0;
    __block int seen_error = 0;

    dispatch_io_t channel = dispatch_io_create(
        DISPATCH_IO_STREAM,
        fd,
        queue,
        ^(int cleanup_err) {
            // dispatch_io calls this once the channel has finished and all
            // outstanding I/O is complete. Close the fd we handed over.
            close(fd);
            (void)cleanup_err;
        }
    );
    if (channel == NULL) {
        dispatch_release(sem);
        close(fd);
        return EIO;
    }

    // Don't split reads into tiny chunks; a single large read submission is
    // what we want on USB-MSC where per-command overhead dominates.
    dispatch_io_set_high_water(channel, SIZE_MAX);
    dispatch_io_set_low_water(channel, length);

    dispatch_io_read(
        channel,
        0,
        length,
        queue,
        ^(bool done, dispatch_data_t data, int io_err) {
            if (io_err && seen_error == 0) {
                seen_error = io_err;
            }
            if (data != NULL) {
                // dispatch_data_t may be a rope of regions; concatenate them.
                dispatch_data_apply(
                    data,
                    ^bool(dispatch_data_t region, size_t region_off,
                          const void *bytes, size_t size) {
                        (void)region;
                        (void)region_off;
                        if (offset + size <= length) {
                            memcpy(out_buf + offset, bytes, size);
                            offset += size;
                        }
                        return true; // continue applying
                    }
                );
            }
            if (done) {
                *out_written = offset;
                dispatch_semaphore_signal(sem);
            }
        }
    );

    // Block until the read completes.
    dispatch_semaphore_wait(sem, DISPATCH_TIME_FOREVER);
    dispatch_release(sem);
    dispatch_release(channel);
    // fd is closed by the cleanup handler above.
    return seen_error;
}
