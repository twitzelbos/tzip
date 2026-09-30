//! io_uring-backed batch file reader (Linux, `io-uring` feature).
//!
//! The default reader issues one blocking `read()` per reader thread, so a
//! thread can have at most one read in flight — fine for spinning media (the
//! FIEMAP extent-order reader already turns that into a forward sweep) but it
//! leaves fast NVMe idle waiting on completions. io_uring lets a single thread
//! keep many reads outstanding: we submit up to [`QUEUE_DEPTH`] reads and reap
//! completions as they land, so the device queue stays full.
//!
//! This composes with the FIEMAP reader: [`crate::linux_raw`] sorts a window
//! of files into physical-disk order and then hands the whole window here, so
//! submissions still go out in disk order while enjoying deep queueing.
//!
//! Completions can arrive out of submission order; the caller carries a stable
//! index through `emit` and the pipeline reorders by it when `--sort` is set
//! (otherwise archive order is arrival order, which is fine).

#![cfg(all(feature = "io-uring", target_os = "linux"))]

use std::io;
use std::os::unix::io::RawFd;

use io_uring::{opcode, types, IoUring};

/// Reads kept in flight at once. 128 is deep enough to saturate NVMe without
/// an unreasonable pinned-buffer footprint.
pub const QUEUE_DEPTH: u32 = 128;

struct JobState {
    buf: Vec<u8>,
    filled: usize,
    len: usize,
}

/// Read every `(fd, len)` in `jobs` fully into an owned buffer, keeping up to
/// [`QUEUE_DEPTH`] reads outstanding. `emit(job_index, buffer)` is invoked as
/// each file finishes (in completion order). Returning `false` from `emit`
/// stops the batch early (e.g. downstream channel closed).
///
/// Callers must keep every `fd` open for the duration of this call.
pub fn read_batch(
    jobs: &[(RawFd, u64)],
    mut emit: impl FnMut(usize, Vec<u8>) -> bool,
) -> io::Result<()> {
    let n = jobs.len();
    if n == 0 {
        return Ok(());
    }
    let mut ring = IoUring::new(QUEUE_DEPTH)?;

    // Buffers are allocated lazily at submit time and freed (taken) on
    // completion, so peak memory is bounded by what's in flight
    // (~QUEUE_DEPTH files) rather than the whole window at once.
    let mut states: Vec<JobState> = jobs
        .iter()
        .map(|&(_, len)| JobState {
            buf: Vec::new(),
            filled: 0,
            len: len as usize,
        })
        .collect();

    // Build a read SQE for job `i` starting at its current fill offset.
    //
    // SAFETY: `states[i].buf` is a stable heap allocation that lives until the
    // job completes (we only `take()` it on completion), and the caller
    // guarantees `fd` stays open for the batch. The pointer + length stay
    // within the buffer.
    let make_sqe = |i: usize, states: &mut [JobState]| -> io_uring::squeue::Entry {
        let (fd, _) = jobs[i];
        let st = &mut states[i];
        if st.buf.is_empty() && st.len > 0 {
            st.buf = vec![0u8; st.len]; // lazy: only allocate when submitting
        }
        let ptr = unsafe { st.buf.as_mut_ptr().add(st.filled) };
        // A single io_uring read length is a u32. For files ≥ 4 GiB this reads
        // the first chunk; the short-read path below resubmits from the new
        // offset until the whole file is in.
        let remaining = (st.len - st.filled).min(u32::MAX as usize) as u32;
        opcode::Read::new(types::Fd(fd), ptr, remaining)
            .offset(st.filled as u64)
            .build()
            .user_data(i as u64)
    };

    let mut next = 0usize; // next job index to submit
    let mut inflight = 0usize;
    let mut done = 0usize;

    while done < n {
        // Top up the submission queue.
        while inflight < QUEUE_DEPTH as usize && next < n {
            if states[next].len == 0 {
                // Empty file — nothing to read; emit immediately.
                if !emit(next, Vec::new()) {
                    return Ok(());
                }
                done += 1;
                next += 1;
                continue;
            }
            let e = make_sqe(next, &mut states);
            // SAFETY: entry references buffers/fds valid for the batch.
            if unsafe { ring.submission().push(&e) }.is_err() {
                break; // SQ full — drain some completions first
            }
            inflight += 1;
            next += 1;
        }

        if inflight == 0 {
            // Nothing outstanding and nothing left to submit.
            break;
        }

        ring.submit_and_wait(1)?;

        // Drain completions into a local buffer so the CQ borrow is released
        // before we push any short-read continuations.
        let mut completions: Vec<(usize, i32)> = Vec::new();
        {
            let cq = ring.completion();
            for cqe in cq {
                completions.push((cqe.user_data() as usize, cqe.result()));
            }
        }

        let mut resubmit: Vec<usize> = Vec::new();
        for (i, res) in completions {
            if res < 0 {
                return Err(io::Error::from_raw_os_error(-res));
            }
            let got = res as usize;
            let st = &mut states[i];
            if got == 0 {
                // Early EOF (file shrank since we stat'd it). Emit what we got.
                let mut buf = std::mem::take(&mut st.buf);
                buf.truncate(st.filled);
                inflight -= 1;
                done += 1;
                if !emit(i, buf) {
                    return Ok(());
                }
                continue;
            }
            st.filled += got;
            if st.filled >= st.len {
                let buf = std::mem::take(&mut st.buf);
                inflight -= 1;
                done += 1;
                if !emit(i, buf) {
                    return Ok(());
                }
            } else {
                // Short read — queue a continuation for the remainder.
                resubmit.push(i);
            }
        }

        for i in resubmit {
            let e = make_sqe(i, &mut states);
            // SAFETY: as above.
            if unsafe { ring.submission().push(&e) }.is_err() {
                ring.submit()?;
                if unsafe { ring.submission().push(&e) }.is_err() {
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        "io_uring submission queue push failed",
                    ));
                }
            }
        }
    }

    Ok(())
}
