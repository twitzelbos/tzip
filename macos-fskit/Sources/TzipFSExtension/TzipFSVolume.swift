//
// TzipFSVolume.swift
//
// Minimal read-only `FSVolume` implementation. Every FSKit callback
// forwards to the Rust `VirtualFs` via the C ABI in `RustABI.swift`.
//
// Status: skeleton — compiles against the FSKit SDK (macOS 15+) and
// implements lookup / getAttributes / enumerateDirectory / readFromFile
// well enough to browse + cat an archive. Mount-syscall-side plumbing
// and System Extension install flow live on the Rust side.
//

import FSKit
import Foundation

/// An item inside a tzip volume. FSKit treats `FSItem` as mostly opaque
/// and uses pointer identity, so we just stash the inode here.
final class TzipFSItem: FSItem {
    let ino: UInt64
    init(ino: UInt64) {
        self.ino = ino
        super.init()
    }
}

final class TzipFSVolume: FSVolume,
    FSVolume.Operations,
    FSVolume.ReadWriteOperations,
    FSVolume.PathConfOperations
{
    // Opaque handle to the Rust VirtualFs; freed on deinit.
    private let handle: TzipVfsHandle
    private let writable: Bool

    init(volumeID: FSVolume.Identifier, volumeName: FSFileName, handle: TzipVfsHandle) {
        self.handle = handle
        self.writable = tzip_is_writable(handle) != 0
        super.init(volumeID: volumeID, volumeName: volumeName)
    }

    deinit {
        tzip_close_volume(handle)
    }

    // MARK: FSVolume.PathConfOperations

    var maximumLinkCount: Int { 1 }
    var maximumNameLength: Int { 1024 }
    var restrictsOwnershipChanges: Bool { true }
    var truncatesLongNames: Bool { false }

    // MARK: FSVolume.CommonOperations required properties

    var supportedVolumeCapabilities: FSVolume.SupportedCapabilities {
        let caps = FSVolume.SupportedCapabilities()
        caps.supportsPersistentObjectIDs = true
        caps.supportsSymbolicLinks = false
        caps.supportsHardLinks = false
        caps.supportsJournal = false
        caps.supportsActiveJournal = false
        caps.doesNotSupportRootTimes = false
        caps.supportsSparseFiles = false
        caps.supportsZeroRuns = false
        caps.supportsFastStatFS = true
        caps.supports2TBFiles = true
        caps.supportsOpenDenyModes = false
        caps.supportsHiddenFiles = false
        caps.doesNotSupportVolumeSizes = true
        caps.supports64BitObjectIDs = true
        caps.supportsDocumentID = false
        return caps
    }

    var volumeStatistics: FSStatFSResult {
        // Archive-backed R/O volume: we don't model block usage. Report
        // zero-everything which tells callers "unknown"; `supportsFastStatFS`
        // above hints the kernel not to cache this answer heavily.
        return FSStatFSResult(fileSystemTypeName: "tzip")
    }

    // MARK: FSVolume.CommonOperations (via FSVolume.Operations)

    func mount(options: FSTaskOptions, replyHandler reply: @escaping (Error?) -> Void) {
        reply(nil)
    }

    func unmount(replyHandler reply: @escaping () -> Void) {
        // Flush any pending writes before the volume vanishes. On a
        // read-only mount this is a no-op in Rust; on a writable mount
        // this rebuilds the archive via pipeline::run(update + atomic).
        commitPendingWrites()
        reply()
    }

    func synchronize(flags: FSSyncFlags,
                     replyHandler reply: @escaping (Error?) -> Void) {
        // Per-volume sync — in-memory writes need an unmount to land, so
        // we accept the sync without side effects. FSKit still calls us
        // eventually at unmount time to run the actual commit.
        reply(nil)
    }

    /// Call `tzip_commit` and log the outcome. Errors are surfaced to
    /// stderr (the FSKit daemon logs these to Console.app) — we can't
    /// fail the unmount cleanly from here, so we yell and move on.
    /// The Rust side guarantees atomic rename: on commit failure the
    /// original archive is untouched.
    private func commitPendingWrites() {
        if !writable { return }
        var dirty: Int = 0
        var tombstoned: Int = 0
        var errPtr: UnsafeMutablePointer<CChar>? = nil
        let rc = tzip_commit(handle, &dirty, &tombstoned, &errPtr)
        if rc != 0 {
            let msg = errPtr.map { String(cString: $0) } ?? "unknown"
            if let e = errPtr { tzip_free_string(e) }
            FileHandle.standardError.write(
                "tzip FSKit: commit FAILED — pending changes not written: \(msg)\n"
                    .data(using: .utf8) ?? Data())
        } else if dirty > 0 || tombstoned > 0 {
            FileHandle.standardError.write(
                "tzip FSKit: committed \(dirty) new/changed entries, \(tombstoned) deletions\n"
                    .data(using: .utf8) ?? Data())
        }
    }

    func reclaimItem(_ item: FSItem,
                     replyHandler reply: @escaping (Error?) -> Void) {
        // We don't hold any per-item state beyond the inode stored on
        // the TzipFSItem itself. Nothing to reclaim; ARC releases on return.
        reply(nil)
    }

    // MARK: FSVolume.Operations core

    func activate(options: FSTaskOptions,
                  replyHandler reply: @escaping (FSItem?, Error?) -> Void) {
        reply(TzipFSItem(ino: tzip_root_ino()), nil)
    }

    func deactivate(options: FSDeactivateOptions,
                    replyHandler reply: @escaping (Error?) -> Void) {
        reply(nil)
    }

    func lookupItem(named name: FSFileName,
                    inDirectory directory: FSItem,
                    replyHandler reply: @escaping (FSItem?, FSFileName?, Error?) -> Void) {
        guard let parent = directory as? TzipFSItem else {
            reply(nil, nil, posixError(EINVAL))
            return
        }
        var attr = CAttr()
        let data = name.data
        let rc = data.withUnsafeBytes { buf -> Int32 in
            let p = buf.bindMemory(to: UInt8.self).baseAddress!
            return tzip_lookup(handle, parent.ino, p, data.count, &attr)
        }
        if rc == -2 { reply(nil, nil, posixError(ENOENT)); return }
        if rc != 0  { reply(nil, nil, posixError(EIO));    return }
        let item = TzipFSItem(ino: attr.ino)
        reply(item, name, nil)
    }

    func getAttributes(_ desiredAttributes: FSItem.GetAttributesRequest,
                       of item: FSItem,
                       replyHandler reply: @escaping (FSItem.Attributes?, Error?) -> Void) {
        guard let it = item as? TzipFSItem else {
            reply(nil, posixError(EINVAL))
            return
        }
        var c = CAttr()
        let rc = tzip_getattr(handle, it.ino, &c)
        if rc != 0 { reply(nil, posixError(ENOENT)); return }
        let out = FSItem.Attributes()
        out.uid = UInt32(getuid())
        out.gid = UInt32(getgid())
        out.mode = c.mode
        out.type = (c.kind == 1) ? .directory : .file
        out.linkCount = 1
        out.flags = 0
        out.size = c.size
        out.allocSize = (c.size + 511) & ~UInt64(511)
        out.fileID = FSItem.Identifier(rawValue: c.ino) ?? FSItem.Identifier.rootDirectory
        let ts = timespec(tv_sec: Int(c.mtime_unix), tv_nsec: 0)
        out.modifyTime = ts
        out.changeTime = ts
        out.accessTime = ts
        out.birthTime = ts
        reply(out, nil)
    }

    func setAttributes(_ newAttributes: FSItem.SetAttributesRequest,
                       on item: FSItem,
                       replyHandler reply: @escaping (FSItem.Attributes?, Error?) -> Void) {
        guard writable else {
            reply(nil, posixError(EROFS))
            return
        }
        guard let it = item as? TzipFSItem else {
            reply(nil, posixError(EINVAL))
            return
        }
        // Only the size field drives a real change — truncate. chmod /
        // chown / timestamp changes are accepted silently to keep
        // `touch`, `cp -p`, and `install -m` from erroring; they just
        // don't persist (the writer emits a fixed mode and now() mtime).
        if newAttributes.isValid(.size) {
            let rc = tzip_truncate(handle, it.ino, newAttributes.size)
            if rc != 0 {
                reply(nil, posixError(errnoFromTzipCode(rc)))
                return
            }
        }
        // Return the item's current attributes.
        var c = CAttr()
        let g = tzip_getattr(handle, it.ino, &c)
        if g != 0 {
            reply(nil, posixError(ENOENT))
            return
        }
        reply(fsAttributes(from: c), nil)
    }

    func createItem(named name: FSFileName,
                    type: FSItem.ItemType,
                    inDirectory directory: FSItem,
                    attributes newAttributes: FSItem.SetAttributesRequest,
                    replyHandler reply: @escaping (FSItem?, FSFileName?, Error?) -> Void) {
        guard writable else {
            reply(nil, nil, posixError(EROFS))
            return
        }
        guard let parent = directory as? TzipFSItem else {
            reply(nil, nil, posixError(EINVAL))
            return
        }
        var attr = CAttr()
        let data = name.data
        let rc: Int32 = data.withUnsafeBytes { buf in
            guard let p = buf.bindMemory(to: UInt8.self).baseAddress else { return Int32(EINVAL) }
            switch type {
            case .file:
                return tzip_create(handle, parent.ino, p, data.count, &attr)
            case .directory:
                return tzip_mkdir(handle, parent.ino, p, data.count, &attr)
            default:
                return Int32(EINVAL)
            }
        }
        if rc != 0 {
            reply(nil, nil, posixError(errnoFromTzipCode(rc)))
            return
        }
        reply(TzipFSItem(ino: attr.ino), name, nil)
    }

    func createSymbolicLink(named name: FSFileName,
                            inDirectory directory: FSItem,
                            attributes newAttributes: FSItem.SetAttributesRequest,
                            linkContents contents: FSFileName,
                            replyHandler reply: @escaping (FSItem?, FSFileName?, Error?) -> Void) {
        // ZIP has no first-class symlink encoding (it fakes them via a
        // Unix-mode external attr), and we don't persist arbitrary
        // mode bits yet. Keep this at EROFS until we do.
        reply(nil, nil, posixError(EROFS))
    }

    func createLink(to item: FSItem,
                    named name: FSFileName,
                    inDirectory directory: FSItem,
                    replyHandler reply: @escaping (FSFileName?, Error?) -> Void) {
        // Hardlinks aren't representable in ZIP.
        reply(nil, posixError(EROFS))
    }

    func renameItem(_ item: FSItem,
                    inDirectory sourceDirectory: FSItem,
                    named sourceName: FSFileName,
                    to destinationName: FSFileName,
                    inDirectory destinationDirectory: FSItem,
                    overItem existingItem: FSItem?,
                    replyHandler reply: @escaping (FSFileName?, Error?) -> Void) {
        // Deferred: rename needs VFS-level support (not yet added to
        // either Linux or macOS write paths). Matches the Linux cut.
        reply(nil, posixError(EROFS))
    }

    func removeItem(_ item: FSItem,
                    named name: FSFileName,
                    fromDirectory directory: FSItem,
                    replyHandler reply: @escaping (Error?) -> Void) {
        guard writable else {
            reply(posixError(EROFS))
            return
        }
        guard let parent = directory as? TzipFSItem,
              let target = item as? TzipFSItem else {
            reply(posixError(EINVAL))
            return
        }
        // Dispatch unlink vs rmdir based on the item's kind.
        var c = CAttr()
        _ = tzip_getattr(handle, target.ino, &c)
        let data = name.data
        let rc: Int32 = data.withUnsafeBytes { buf in
            guard let p = buf.bindMemory(to: UInt8.self).baseAddress else { return Int32(EINVAL) }
            if c.kind == 1 {
                return tzip_rmdir(handle, parent.ino, p, data.count)
            } else {
                return tzip_unlink(handle, parent.ino, p, data.count)
            }
        }
        if rc != 0 {
            reply(posixError(errnoFromTzipCode(rc)))
            return
        }
        reply(nil)
    }

    func enumerateDirectory(_ directory: FSItem,
                            startingAt cookie: FSDirectoryCookie,
                            verifier: FSDirectoryVerifier,
                            attributes: FSItem.GetAttributesRequest?,
                            packer: FSDirectoryEntryPacker,
                            replyHandler reply: @escaping (FSDirectoryVerifier, Error?) -> Void) {
        guard let dir = directory as? TzipFSItem else {
            reply(verifier, posixError(ENOTDIR))
            return
        }

        // Fetch all children in one shot. For big directories this could
        // blow memory; a follow-up can page via `cookie`.
        let MAX: Int = 4096
        let buf = UnsafeMutablePointer<CDirEntry>.allocate(capacity: MAX)
        defer { buf.deallocate() }
        var n: Int = 0
        let rc = tzip_readdir(handle, dir.ino, buf, MAX, &n)
        if rc != 0 {
            reply(verifier, posixError(ENOENT))
            return
        }

        var nextCookie: FSDirectoryCookie = cookie
        let startIndex = Int(cookie.rawValue)
        for i in startIndex..<n {
            let ent = buf[i]
            defer { tzip_free_string(ent.name) }
            let nameStr = String(cString: ent.name!)
            let name = FSFileName(string: nameStr)
            let itemType: FSItem.ItemType = (ent.kind == 1) ? .directory : .file
            let itemID = FSItem.Identifier(rawValue: ent.ino) ?? FSItem.Identifier.invalid
            nextCookie = FSDirectoryCookie(rawValue: UInt64(i + 1))
            let ok = packer.packEntry(name: name,
                                      itemType: itemType,
                                      itemID: itemID,
                                      nextCookie: nextCookie,
                                      attributes: nil)
            if !ok {
                break
            }
        }

        // Verifier of 1 — our directory tree is immutable for the life
        // of the mount, so a constant verifier is accurate.
        reply(FSDirectoryVerifier(rawValue: 1), nil)
    }

    func readSymbolicLink(_ item: FSItem,
                          replyHandler reply: @escaping (FSFileName?, Error?) -> Void) {
        reply(nil, posixError(EINVAL))
    }

    // MARK: FSVolume.ReadWriteOperations

    func read(from item: FSItem,
              at offset: off_t,
              length: Int,
              into buffer: FSMutableFileDataBuffer,
              replyHandler reply: @escaping (Int, Error?) -> Void) {
        guard let it = item as? TzipFSItem else {
            reply(0, posixError(EINVAL))
            return
        }
        let size = UInt32(min(length, Int(UInt32.max)))
        var written: Int = 0
        var rc: Int32 = -1
        buffer.withUnsafeMutableBytes { raw in
            guard let p = raw.bindMemory(to: UInt8.self).baseAddress else { return }
            rc = tzip_read(handle, it.ino, UInt64(offset), size, p, &written)
        }
        if rc != 0 {
            reply(0, posixError(EIO))
            return
        }
        reply(written, nil)
    }

    func write(contents: Data,
               to item: FSItem,
               at offset: off_t,
               replyHandler reply: @escaping (Int, Error?) -> Void) {
        guard writable else {
            reply(0, posixError(EROFS))
            return
        }
        guard let it = item as? TzipFSItem else {
            reply(0, posixError(EINVAL))
            return
        }
        var written: Int = 0
        var rc: Int32 = -1
        contents.withUnsafeBytes { raw in
            guard let p = raw.bindMemory(to: UInt8.self).baseAddress else { return }
            rc = tzip_write(handle, it.ino, UInt64(offset), p, contents.count, &written)
        }
        if rc != 0 {
            reply(0, posixError(errnoFromTzipCode(rc)))
            return
        }
        reply(written, nil)
    }

    // MARK: Attribute helpers

    private func fsAttributes(from c: CAttr) -> FSItem.Attributes {
        let out = FSItem.Attributes()
        out.uid = UInt32(getuid())
        out.gid = UInt32(getgid())
        out.mode = c.mode
        out.type = (c.kind == 1) ? .directory : .file
        out.linkCount = 1
        out.flags = 0
        out.size = c.size
        out.allocSize = (c.size + 511) & ~UInt64(511)
        out.fileID = FSItem.Identifier(rawValue: c.ino) ?? FSItem.Identifier.rootDirectory
        let ts = timespec(tv_sec: Int(c.mtime_unix), tv_nsec: 0)
        out.modifyTime = ts
        out.changeTime = ts
        out.accessTime = ts
        out.birthTime = ts
        return out
    }
}

// MARK: - Helpers

private func posixError(_ code: Int32) -> NSError {
    NSError(domain: NSPOSIXErrorDomain, code: Int(code), userInfo: nil)
}

/// Translate a tzip ABI return code (negative errno) to a positive
/// errno suitable for `posixError(_:)`. Unknown codes fall through to
/// EIO so the caller sees a non-zero reply rather than a spurious
/// success.
private func errnoFromTzipCode(_ rc: Int32) -> Int32 {
    if rc < 0 {
        let positive = -rc
        if positive > 0 { return positive }
    }
    return EIO
}
