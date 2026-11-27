# Signing tzip for macOS FSKit distribution

End-to-end walkthrough of every Apple account, cert, capability,
and profile you need to produce a `tzip.app` that stock
SIP-enabled Macs will install and enable without user surgery.

Target reader: someone picking this up cold (future-you,
collaborator, IT admin). No prior macOS codesigning experience
assumed.

> **Why this is long.** Shipping a macOS FSKit extension outside
> the App Store is three nested signing dances — the host app,
> the `.appex` inside it, and the notarisation submission — plus
> two restricted entitlements that each need a separate
> provisioning profile. This doc makes the full chain explicit
> because the error messages Apple surfaces along the way are
> famously opaque.

## What you need (checklist)

```
1. Apple Developer Program membership ($99/year, individual or organization)
2. "Developer ID Application" certificate — installed in your login keychain
3. Two Apple App IDs registered in the Developer Portal:
   • Host         (default: io.qbio.tzip)
   • Extension    (default: io.qbio.tzip.fskit, with FSKit Module + FSKit Mounter capabilities)
4. Two Developer ID System Extension provisioning profiles — one per App ID
5. An app-specific password for notarytool, stored in your login keychain
   under the profile name "notarytool-profile"
6. Xcode CLI tools (swiftc, codesign, xcrun notarytool, stapler) — present
   if you've ever opened Xcode
```

Everything else (the entitlements, Info.plists, build pipeline) is
in-tree. You can run through this once in 20–30 minutes if you
already have a Developer account; another 10 minutes if you don't.

## Step 1 — Apple Developer Program

Sign in at <https://developer.apple.com>. If you don't already have
an active Program membership, join (`$99/year`, individual works
fine). Macs can produce *development* builds without Program
membership, but only Program members can get the Developer ID
Application cert needed for notarisable distribution.

## Step 2 — Developer ID Application certificate

The portal's **Certificates** section → `+` → **Developer ID
Application** → follow the CSR instructions. Once issued, download
and double-click the `.cer` to install it in your login keychain.

Confirm it's installed:

```sh
security find-identity -v -p codesigning
#  … "Developer ID Application: Your Name (YOURTEAMID)" …
```

Note the 10-char **team identifier** in parentheses — you'll need
it everywhere.

> **Apple Development vs Apple Distribution vs Developer ID
> Application.** Only `Developer ID Application` works here. The
> other two cert types pair with different profile types that
> can't sign a system-extension host for Gatekeeper-approved
> distribution.

## Step 3 — Register two App IDs

Portal → **Identifiers** → `+` → **App IDs** → **App**. Do this
twice.

**3a. Host app id** (default `io.qbio.tzip`; override via
`$TZIP_HOST_BUNDLE_ID` at build time).

- Description: `tzip CLI`
- Bundle ID: **Explicit** → `io.qbio.tzip`
- Capabilities: none required here. The restricted entitlement
  `com.apple.developer.system-extension.install` is granted by
  the profile type, not an App-ID capability.
- Save.

**3b. FSKit extension app id** (default `io.qbio.tzip.fskit`;
override via `$TZIP_FSKIT_BUNDLE_ID`).

- Description: `tzip FSKit extension`
- Bundle ID: **Explicit** → `io.qbio.tzip.fskit`
- Capabilities: **FSKit Module** AND **FSKit Mounter** (both
  checkboxes).
- Save.

## Step 4 — Register your Mac as a device (dev only)

You only need this for *development* profiles, which we don't use —
Developer ID System Extension profiles don't bind to specific
devices (they have `ProvisionsAllDevices = true`). Skip.

## Step 5 — Create two provisioning profiles

Portal → **Profiles** → `+` → scroll to the **Developer ID**
section → **Developer ID System Extension**.

Create two:

**5a. Host profile** pairing cert `Developer ID Application` with
App ID `io.qbio.tzip`.
- Name: `tzip Host SystemExt` (any name)
- Download → stash in `~/Downloads`.

**5b. Extension profile** pairing cert `Developer ID Application`
with App ID `io.qbio.tzip.fskit`.
- Name: `tzip FSKit SystemExt`
- Download → stash in `~/Downloads`.

Verify each profile looks right:

```sh
security cms -D -i ~/Downloads/tzip_Host_SystemExt.provisionprofile \
  | grep -A1 "application-identifier\|developer.system-extension.install"
security cms -D -i ~/Downloads/tzip_FSKit_SystemExt.provisionprofile \
  | grep -A1 "application-identifier\|developer.fskit"
```

The host profile must carry `com.apple.developer.system-extension.install`;
the extension profile must carry `com.apple.developer.fskit.fsmodule`.

## Step 6 — notarytool keychain profile

Notarisation needs an app-specific password associated with your
Apple ID. Generate one at <https://account.apple.com> →
**Sign-In and Security** → **App-Specific Passwords** → `+`.
Label it `notarytool`; copy the generated `xxxx-xxxx-xxxx-xxxx`.

Store it in the keychain under a named profile so build scripts
can use it without ever seeing the password:

```sh
xcrun notarytool store-credentials "notarytool-profile" \
  --apple-id "you@example.com" \
  --team-id  "YOURTEAMID"
# Prompts for the app-specific password you just generated.
```

Verify:

```sh
xcrun notarytool history --keychain-profile notarytool-profile
# Successfully received submission history. (may be empty on first run — that's fine)
```

## Step 7 — Build + sign

```sh
cargo build --release --features fskit
DEVELOPER_ID_APPLICATION="Developer ID Application: Your Name (YOURTEAMID)" \
TZIP_PROVISION_PROFILE_HOST=~/Downloads/tzip_Host_SystemExt.provisionprofile \
TZIP_PROVISION_PROFILE_APPEX=~/Downloads/tzip_FSKit_SystemExt.provisionprofile \
./scripts/build-fskit-bundle.sh
```

Fork users with a non-default bundle namespace also export:

```sh
TZIP_HOST_BUNDLE_ID=com.yourco.tzip
TZIP_FSKIT_BUNDLE_ID=com.yourco.tzip.fskit
```

Those get substituted into every tracked Info.plist, the Rust
`BUNDLE_ID` constant, and the entitlements templates
(`scripts/tzip-*.entitlements.in`). You don't patch tracked
source.

Output: `target/release/tzip.app` — Developer-ID signed,
hardened-runtime, provisioned, ready for notarisation.

## Step 8 — Notarise + staple

```sh
./scripts/notarize-fskit-bundle.sh
```

Submits `target/release/tzip.app.zip` to Apple, waits for
`Accepted` (typically 2–5 min), staples the ticket. If it comes
back `Invalid`, the script prints the submission id — run
`xcrun notarytool log <id> --keychain-profile notarytool-profile`
for Apple's rejection detail.

## Step 9 — Install

```sh
cp -R target/release/tzip.app /Applications/tzip.app
open 'x-apple.systempreferences:com.apple.LoginItems-Settings.extension'
#  → scroll to "tzip Extensions" → flip "FSKit Modules" ON
```

Then:

```sh
/Applications/tzip.app/Contents/MacOS/tzip \
    mount s3://bucket/archive.zip /tmp/mnt -p PASSWORD
```

## Verifying the signing chain at each step

```sh
# Developer ID signature chain visible
codesign -dvvv target/release/tzip.app | grep -E "Authority|TeamIdentifier"

# Entitlements on the host app (should list system-extension.install)
codesign -d --entitlements - target/release/tzip.app

# Entitlements on the .appex (should list fskit.fsmodule + sandbox)
codesign -d --entitlements - target/release/tzip.app/Contents/PlugIns/tzip.appex

# Notarisation stapled (should say "The validate action worked!")
xcrun stapler validate target/release/tzip.app

# Launch Services picked up the extension (dumps all bundles)
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister \
    -dump | grep -A2 "io.qbio.tzip.fskit"
```

## Reading AMFI errors (when something's wrong)

AMFI is the kernel component that validates code signing +
provisioning at exec time. Every rejection goes to the unified
log; find the most recent one with:

```sh
/usr/bin/log show --last 2m --predicate 'process == "amfid"' \
    | grep -iE "tzip|not valid"
```

Common errors and what they actually mean:

| AMFI error                                        | Cause                                                                                                         |
|---------------------------------------------------|---------------------------------------------------------------------------------------------------------------|
| `Code -413 "No matching profile found"`           | Binary declares a restricted entitlement, but no embedded profile covers it. Either missing or wrong App ID. |
| `Code -413 "Unsatisfied entitlements"`            | The embedded profile covers some, but not all, of the restricted entitlements the binary declares.           |
| `"Provisioning profile does not allow this device."` | Dev profile with specific devices, used on a Mac whose UDID isn't registered. (Developer ID profiles don't have this.) |
| `"code signature validation failed fatally"`      | Signature is malformed or the signing cert isn't trusted. Re-sign with Developer ID (not ad-hoc).            |

For "mount succeeds but immediately unmounts" or "File System
Extensions toggle snaps back":

```sh
/usr/bin/log show --last 2m --predicate 'process == "fskitd" OR process == "fskit_agent"' \
    | grep -iE "tzip|twitzel|module"
```

## Enterprise MDM gotcha

On an MDM-managed Mac (JumpCloud, Jamf, Kandji…), the
`com.apple.servicemanagement` Managed Login Items payload often
allowlists one specific team identifier (your corp security
vendor). The FSKit Modules toggle will snap back with no prompt
for any team not on that list.

Check for a blocking policy:

```sh
plutil -p /Library/Managed\ Preferences/`whoami`/com.apple.servicemanagement.plist
```

If `Rules` contains only one or two `TeamIdentifier`s that aren't
yours, your IT/MDM admin has to add your team id (or exempt this
Mac). That's the gate — not your code.

See `macos-fskit/README.md` for the companion rationale, and the
WebDAV-based userspace mount path as a follow-up that skips this
gate entirely.
