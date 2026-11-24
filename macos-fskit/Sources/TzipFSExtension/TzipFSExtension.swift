//
// TzipFSExtension.swift
//
// Top-level FSKit extension class. Subclasses `FSUnaryFileSystem` and
// conforms to `FSUnaryFileSystemOperations` to implement probe / load /
// unload. The actual volume behaviour (lookup, readdir, read…) lives in
// `TzipFSVolume`.
//
// FSKit invokes `loadResource(resource:options:)` when the user runs
// `mount -t tzip <src> <mnt> -o <opts>`. The options array carries our
// archive URL + Keychain password tag + cache budget.
//

import FSKit
import Foundation

// MARK: - Extension entry point
//
// The FSKit extension is loaded by the system via Info.plist's
// `NSPrincipalClass = TzipFSExtension.TzipFSExtension`. There's no
// classic `@main` entry point — the FSKit daemon instantiates the
// class and drives its protocol callbacks directly.

final class TzipFSExtension: FSUnaryFileSystem, FSUnaryFileSystemOperations {

    // MARK: Required

    /// Short identifier for the filesystem. Must match the `-t` argument
    /// to `mount` and the Info.plist's `FSFileSystemName`.
    static let fileSystemName: String = "tzip"

    // MARK: FSUnaryFileSystemOperations

    func probeResource(
        resource: FSResource,
        replyHandler reply: @escaping (FSProbeResult?, Error?) -> Void
    ) {
        // tzip doesn't currently inspect the resource — the real signal
        // of validity is whether `loadResource` can open the archive
        // URL from the mount options. Reply "recognized" so FSKit lets
        // the load phase proceed.
        //
        // TODO: inspect the resource type (block device vs. path URL)
        // and short-circuit obviously wrong ones.
        // Build a container ID. For an archive-backed mount we don't have
        // a durable identifier on disk; FSKit says to use a random UUID
        // in that case.
        let cid = FSContainerIdentifier(uuid: UUID())
        let result = FSProbeResult.recognized(name: "tzip", containerID: cid)
        reply(result, nil)
    }

    func loadResource(
        resource: FSResource,
        options: FSTaskOptions,
        replyHandler reply: @escaping (FSVolume?, Error?) -> Void
    ) {
        // Mount options arrive here as `options.taskOptions` — a `[String]`
        // parallel to argv. We expect something like:
        //   -o url=https%3A%2F%2Fhost%2Farchive.zip,cache_mb=256,keychain=<tag>
        // which FSKit splits into individual option strings.
        let parsed = parseMountOptions(options.taskOptions)

        guard let archiveURL = parsed.archiveURL else {
            reply(nil, NSError(domain: NSPOSIXErrorDomain, code: Int(EINVAL),
                               userInfo: [NSLocalizedDescriptionKey:
                                "tzip mount requires -o url=<archive>"]))
            return
        }

        // Resolve the Keychain password tag to a plaintext password string
        // (if present). The password lives in the user's Keychain under
        // service "tzip-mount" + account <tag>.
        let password: String?
        if let tag = parsed.keychainTag {
            password = readKeychainPassword(service: "tzip-mount", account: tag)
        } else {
            password = nil
        }

        var handle = TzipVfsHandle(inner: nil)
        var errPtr: UnsafeMutablePointer<CChar>? = nil
        let urlCstr = archiveURL.cString(using: .utf8)!
        let pwCstr = password?.cString(using: .utf8)
        // Dispatch to the writable or read-only Rust constructor based on
        // the `write` mount option. The RW path rejects remote URLs on
        // its own (commit needs atomic rename on a real path).
        let openFn = parsed.write ? tzip_open_volume_rw : tzip_open_volume
        let rc = urlCstr.withUnsafeBufferPointer { urlBuf -> Int32 in
            if let pw = pwCstr {
                return pw.withUnsafeBufferPointer { pwBuf in
                    openFn(urlBuf.baseAddress!,
                           pwBuf.baseAddress,
                           UInt32(parsed.cacheMb),
                           parsed.verbose ? 1 : 0,
                           &handle,
                           &errPtr)
                }
            } else {
                return openFn(urlBuf.baseAddress!,
                              nil,
                              UInt32(parsed.cacheMb),
                              parsed.verbose ? 1 : 0,
                              &handle,
                              &errPtr)
            }
        }
        if rc != 0 {
            let msg: String
            if let e = errPtr {
                msg = String(cString: e)
                tzip_free_string(e)
            } else {
                msg = "unknown error opening archive"
            }
            reply(nil, NSError(domain: NSPOSIXErrorDomain, code: Int(-rc),
                               userInfo: [NSLocalizedDescriptionKey: msg]))
            return
        }

        let volumeName = FSFileName(string: "tzip")
        let volumeID = FSVolume.Identifier(uuid: UUID())
        let vol = TzipFSVolume(volumeID: volumeID,
                               volumeName: volumeName,
                               handle: handle)
        reply(vol, nil)
    }

    func unloadResource(
        resource: FSResource,
        options: FSTaskOptions,
        replyHandler reply: @escaping (Error?) -> Void
    ) {
        // FSKit calls unmount + reclaim all items before this. Volume's
        // deinit frees the TzipVfsHandle via tzip_close_volume.
        reply(nil)
    }
}

// MARK: - Mount options parsing

private struct ParsedOptions {
    var archiveURL: String? = nil
    var keychainTag: String? = nil
    var cacheMb: Int = 256
    var allowOther: Bool = false
    var verbose: Bool = false
    /// Open the volume read-write. Only honored for local archives —
    /// the Rust side rejects remote URLs with a loud error.
    var write: Bool = false
}

private func parseMountOptions(_ opts: [String]) -> ParsedOptions {
    var out = ParsedOptions()
    for opt in opts {
        // Each `opt` may be a single flag or `k=v`. Also handle a
        // leading `-o ` that we strip.
        let s = opt.hasPrefix("-o") ? String(opt.dropFirst(2)).trimmingCharacters(in: .whitespaces) : opt
        for pair in s.split(separator: ",") {
            let parts = pair.split(separator: "=", maxSplits: 1).map(String.init)
            let key = parts[0]
            let val = parts.count > 1 ? parts[1] : ""
            switch key {
            case "url":
                out.archiveURL = val.removingPercentEncoding ?? val
            case "keychain":
                out.keychainTag = val
            case "cache_mb":
                out.cacheMb = Int(val) ?? 256
            case "allow_other":
                out.allowOther = true
            case "verbose":
                out.verbose = true
            case "write":
                out.write = true
            default:
                break
            }
        }
    }
    return out
}

// MARK: - Keychain read (mount side)

/// Fetch a generic-password item from the user's keychain.
private func readKeychainPassword(service: String, account: String) -> String? {
    let query: [String: Any] = [
        kSecClass as String: kSecClassGenericPassword,
        kSecAttrService as String: service,
        kSecAttrAccount as String: account,
        kSecReturnData as String: true,
        kSecMatchLimit as String: kSecMatchLimitOne,
    ]
    var out: AnyObject?
    let status = SecItemCopyMatching(query as CFDictionary, &out)
    guard status == errSecSuccess, let data = out as? Data else {
        return nil
    }
    return String(data: data, encoding: .utf8)
}
