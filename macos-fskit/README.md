# tzip FSKit extension

The macOS 15.4+ FSKit extension that makes `tzip mount s3://… /mnt`
work. Written as an ExtensionKit XPC service (`@main` entry point in
`TzipAppExtension.swift`), packaged as `.app` + nested `.appex` under
`tzip.app/Contents/PlugIns/`, and signed with Developer ID so stock
SIP-enabled Macs can install it without the user disabling anything.

## Shape

```
tzip.app/
  Contents/
    Info.plist                        host app (CFBundleIdentifier: io.qbio.tzip)
    MacOS/tzip                        the Rust CLI (fskit feature)
    Frameworks/libtzip.dylib          Rust cdylib (tzip_* ABI symbols)
    embedded.provisionprofile         host profile — com.apple.developer.system-extension.install
    PlugIns/
      tzip.appex/
        Contents/
          Info.plist                  XPC!, EXAppExtensionAttributes
          MacOS/TzipFSExtension       Swift exec (@main, ExtensionKit)
          Frameworks/libtzip.dylib    same Rust cdylib (resolved via @rpath)
          embedded.provisionprofile   .appex profile — com.apple.developer.fskit.fsmodule
```

Key architectural points:

- **ExtensionKit, not NSExtension.** macOS 15.4 moved FSKit to
  ExtensionKit's XPC-service model. The extension is a standalone
  Mach-O executable with `@main`, not a dylib that gets loaded into
  a host process. `CFBundlePackageType = XPC!` and the metadata
  lives under `EXAppExtensionAttributes.EXExtensionPointIdentifier`.
- **`@rpath/libtzip.dylib`.** The Swift executable links against the
  Rust cdylib at Swift-build time (via `TZIP_RUST_LIBDIR` →
  `Package.swift`'s `-L` + `-ltzip`). `scripts/build-fskit-bundle.sh`
  rewrites the LC_LOAD_DYLIB after the build so the runtime path
  resolves via rpath from inside the `.appex`.
- **Nested provisioning profiles.** Both the host and the `.appex`
  carry a restricted entitlement, so each needs its own embedded
  Developer ID profile. AMFI validates `com.apple.application-identifier`
  against the profile's `application-identifier`; mismatches get
  `Error -413 "No matching profile found"`.

## Build

```sh
# From the repo root. Default bundle ids: io.qbio.tzip + io.qbio.tzip.fskit.
cargo build --release --features fskit
DEVELOPER_ID_APPLICATION="Developer ID Application: You (TEAMID)" \
TZIP_PROVISION_PROFILE_HOST=~/Downloads/tzip_Host.provisionprofile \
TZIP_PROVISION_PROFILE_APPEX=~/Downloads/tzip_FSKit.provisionprofile \
./scripts/build-fskit-bundle.sh
./scripts/notarize-fskit-bundle.sh
```

Forks with a different Apple Developer namespace:

```sh
TZIP_HOST_BUNDLE_ID=com.yourco.tzip TZIP_FSKIT_BUNDLE_ID=com.yourco.tzip.fskit ./scripts/build-fskit-bundle.sh
```

The bundle id substitutes into the host Info.plist, the .appex
Info.plist, the Rust `BUNDLE_ID` constant (`option_env!` at
compile time), and the entitlements templates
(`scripts/tzip-*.entitlements.in`).

## Developer Portal setup (one-time)

Apple requires provisioning profiles for the two restricted
entitlements this extension uses. The portal does this self-serve —
no Apple review.

1. Get a **Developer ID Application** cert (not Apple Development)
   on your Mac. `security find-identity -v -p codesigning` should
   list it.
2. developer.apple.com → **Identifiers → `+` → App IDs**. Create
   both:
   - Explicit `io.qbio.tzip` (or your host id). Capabilities: none
     specific; the restricted entitlement
     `com.apple.developer.system-extension.install` is granted by
     the profile type.
   - Explicit `io.qbio.tzip.fskit` (or your fskit id). Capabilities:
     check **FSKit Module** and **FSKit Mounter**.
3. **Profiles → `+` → Developer ID**. Create two:
   - Developer ID **System Extension** profile for the host id.
   - Developer ID **System Extension** profile for the fskit id.
   Both target your Developer ID Application cert. Developer ID
   profiles aren't device-locked (`ProvisionsAllDevices = true`),
   so these work on any Mac your binary ends up on.
4. Download both `.provisionprofile` files. Pass their paths via
   `TZIP_PROVISION_PROFILE_HOST` + `TZIP_PROVISION_PROFILE_APPEX`
   when running `scripts/build-fskit-bundle.sh`.

## Notarisation

`scripts/notarize-fskit-bundle.sh` submits the whole `.app` to
Apple notarytool, waits, and staples. Requires a keychain profile:

```sh
xcrun notarytool store-credentials "notarytool-profile" \
  --apple-id "you@example.com" \
  --team-id  "TEAMID"
# (enter app-specific password from appleid.apple.com)
```

Then:

```sh
./scripts/notarize-fskit-bundle.sh
```

Typical time: 2–5 minutes.

## Install + enable

```sh
cp -R target/release/tzip.app /Applications/tzip.app
open 'x-apple.systempreferences:com.apple.LoginItems-Settings.extension'
# → scroll to "tzip Extensions" → flip "FSKit Modules" on
/Applications/tzip.app/Contents/MacOS/tzip mount s3://bucket/key.zip /tmp/mnt -p PASSWORD
```

## Enterprise MDM gotcha

On MDM-managed Macs (JumpCloud, Jamf, Kandji, …), the
`com.apple.servicemanagement` Managed Login Items payload often
allowlists a specific Team Identifier. Enabling the FSKit toggle
for any team **not** on that list silently fails — the toggle
snaps back with no prompt. Fix is one of:

- IT admin adds your team id to the allowlist, or
- Use an unmanaged Mac for development, or
- Ship to end users whose Macs allowlist your team.

See also the companion WebDAV-based userspace mount path (tracked
as a follow-up) — it skips the system-extension gate entirely by
proxying through macOS's built-in `mount_webdav` client.

## Why this is non-trivial compared to Linux FUSE3

Linux's FUSE3 path is: install `fuse3`, implement the operations,
`tzip mount` just works — no code-signing, no provisioning, no
system-extension approval. macOS deprecated kexts and macFUSE for
exactly the trust-boundary reasons that then make FSKit extensions
require this much ceremony. Apple's framework is clean; the
distribution surface around it is strict.
