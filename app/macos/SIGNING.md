# Signing and notarisation status

Any published Hauksbee.app is **signed with a Developer ID identity and
notarised** (ticket stapled), so a normal download opens with a plain
double-click and no Gatekeeper warning. `spctl --assess --type execute` on a
quarantined release bundle reports `accepted, source=Notarized Developer ID`.
The release workflow builds and publishes the app zip only when the signing
secrets it lists are configured; a release cut without them ships unsigned
darwin tarballs and no app zip at all, which keeps that guarantee structural.

`build-app.sh` does the signing and notarisation when given credentials (see
"How to produce a signed build" below; the release flow uses the `notary`
notarytool keychain profile). A build made WITHOUT credentials is unsigned and
Gatekeeper will warn on first launch; that is expected for local dev builds
and honest: we do not fake, self-sign, or strip quarantine on the user's
behalf.

## What users of an unsigned dev build do

First launch only:

1. In Finder, **right-click (or Control-click) Hauksbee.app and choose Open**.
2. The Gatekeeper dialog now shows an **Open** button; click it.
3. Every later launch is a normal double-click.

On macOS Sequoia and later the right-click route may be removed; the fallback
is **System Settings > Privacy & Security**, scroll to the blocked-app notice,
and click **Open Anyway**.

## How to produce a signed build

`build-app.sh` reads these env vars; all are optional, and an unsigned build
needs none of them:

| Variable | Meaning |
| -------- | ------- |
| `HAUKSBEE_SIGN_IDENTITY` | A "Developer ID Application: NAME (TEAMID)" identity present in the build keychain. Triggers inside-out codesigning (hardened runtime, secure timestamp) of both binaries, the launcher, and the bundle, followed by `codesign --verify --deep --strict` and an `spctl --assess` whose verdict is printed as-is. |
| `HAUKSBEE_NOTARY_PROFILE` | A `notarytool` keychain profile (created once with `xcrun notarytool store-credentials`). Triggers `notarytool submit --wait` + `stapler staple`. Requires `HAUKSBEE_SIGN_IDENTITY`. |
| `HAUKSBEE_NOTARY_APPLE_ID` / `HAUKSBEE_NOTARY_TEAM_ID` / `HAUKSBEE_NOTARY_PASSWORD` | Direct notary credentials (Apple ID, team ID, app-specific password) as an alternative to the profile. All three together. |

Example, fully signed and notarised:

```sh
export HAUKSBEE_SIGN_IDENTITY="Developer ID Application: Example Corp (ABCDE12345)"
export HAUKSBEE_NOTARY_PROFILE="hauksbee-notary"
app/macos/build-app.sh --no-build --version 0.1.0 --target darwin-arm64 --out dist
```

Note on `spctl`: a signed but NOT-yet-notarised app is still **rejected** by
Gatekeeper policy; the script prints that rejection rather than pretending
otherwise. Only after notarisation + stapling does `spctl --assess` accept.

## Tarball binaries

The plain darwin tarballs get the same treatment from `scripts/bundle.sh`,
driven by the same env vars: `HAUKSBEE_SIGN_IDENTITY` codesigns the three
staged binaries (`hauksbee`, `hauksbee-ci`, `hauksbee-mcp`) with hardened
runtime and a secure timestamp, then verifies each with `codesign --verify
--strict`; the notary credentials (`HAUKSBEE_NOTARY_PROFILE`, or the
`HAUKSBEE_NOTARY_APPLE_ID` / `HAUKSBEE_NOTARY_TEAM_ID` /
`HAUKSBEE_NOTARY_PASSWORD` triple) trigger a `notarytool submit --wait` of a
zip of the signed binaries. Without the vars the tarball ships unsigned, with
a printed note naming them.

One difference from the app: a bare Mach-O cannot carry a stapled ticket
(`stapler` staples bundles, disk images and packages, not standalone
executables), so there is nothing to staple after the submission is accepted.
The ticket lives on Apple's servers, and Gatekeeper looks it up **online** the
first time a quarantined binary runs. An offline first run of a
browser-downloaded tarball binary therefore falls back to the unsigned-build
flow above; a copy installed by `get-hauksbee.sh` or `install.sh` never
carries the quarantine flag in the first place.

The release workflow's "Verify the two shapes" step runs `codesign --verify
--strict` on every binary inside both darwin tarballs and fails the job if any
is unsigned. Notarisation of the tarball binaries is required at launch, on
the same terms as the app gate.

## What the signing setup needs (one-time)

To make the warning disappear entirely the release pipeline needs:

1. **An Apple Developer Program membership** (USD 99/year) for the org that
   ships hauksbee.
2. **A Developer ID Application certificate** issued from that account and
   exported as a password-protected `.p12`. Configure these exact protected
   GitHub repository/environment secrets for the release workflow:

   | Secret | Value |
   | ------ | ----- |
   | `HAUKSBEE_SIGN_IDENTITY` | The complete identity shown by `security find-identity`, for example `Developer ID Application: Example Corp (ABCDE12345)`. |
   | `HAUKSBEE_SIGNING_CERTIFICATE_BASE64` | The `.p12` bytes encoded with `base64` (line wrapping is acceptable). |
   | `HAUKSBEE_SIGNING_CERTIFICATE_PASSWORD` | The password used when exporting the `.p12`. |
   | `HAUKSBEE_NOTARY_APPLE_ID` | Apple ID used by `notarytool`. |
   | `HAUKSBEE_NOTARY_TEAM_ID` | Apple Developer team identifier. |
   | `HAUKSBEE_NOTARY_PASSWORD` | An app-specific password for that Apple ID. |

   The macOS release job fails before compiling if any of these six secrets is
   missing. It imports the decoded certificate into a throw-away keychain,
   adds that keychain to the runner's search list, and restores the original
   list and deletes both the keychain and decoded `.p12` in an always-run
   cleanup step. The certificate and passwords never enter `GITHUB_ENV` or an
   artifact, and the workflow does not echo the import command or its values.
   Ordinary local builds do not run this import step and remain unsigned unless
   the local signing variables below are supplied.
3. **Code signing** of every Mach-O in the bundle, inside-out, with hardened
   runtime:

   ```sh
   codesign --force --options runtime --timestamp \
     --sign "Developer ID Application: <NAME> (<TEAMID>)" \
     Hauksbee.app/Contents/Resources/bin/hauksbee \
     Hauksbee.app/Contents/Resources/bin/hauksbee-ci \
     Hauksbee.app/Contents/Resources/bin/hauksbee-mcp
   codesign --force --options runtime --timestamp \
     --sign "Developer ID Application: <NAME> (<TEAMID>)" Hauksbee.app
   ```

4. **Notarisation** of the zipped app with `notarytool`, then stapling:

   ```sh
   ditto -c -k --keepParent Hauksbee.app Hauksbee.zip
   xcrun notarytool submit Hauksbee.zip \
     --apple-id <ID> --team-id <TEAMID> --password <APP-SPECIFIC-PW> --wait
   xcrun stapler staple Hauksbee.app
   ```

   (Then re-zip the stapled bundle; the stapled ticket must be inside the
   distributed archive.)

5. A hardened-runtime audit: `hauksbee serve` binds a local port and spawns
   `hauksbee-ci`; neither needs entitlements beyond the defaults, so no
   entitlements plist is expected, but verify after the first signed build
   with `codesign --verify --deep --strict` and `spctl -a -vv`.

Until those pieces are configured in the release environment, the workflow
releases in unsigned mode: plain darwin tarballs, no app zip. Once they are
configured it fails closed rather than publish an unsigned macOS asset, and a
partial configuration fails the job outright. Local unsigned builds remain
supported and are documented above.

## Release acceptance

The release workflow signs the nested Mach-Os under
`Contents/Resources/bin` before signing `Hauksbee.app`, then submits the app
for notarisation and staples the accepted ticket. The app gate runs
`codesign --verify --deep --strict`, `spctl`, and `xcrun stapler validate`; the
tarball gate verifies every command-line binary with `codesign --verify
--strict`. Any missing secret, signing failure, notarisation rejection, or
stapling failure stops publication. External signing wrappers must include the
three binaries under `Contents/Resources/bin`, not only the launcher under
`Contents/MacOS`.
