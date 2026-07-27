# Signing and notarisation status

Release builds of Hauksbee.app are currently **unsigned and not notarised**.
macOS Gatekeeper will therefore warn on first launch that the app is from an
unidentified developer. This is expected and honest: we do not fake,
self-sign, or strip quarantine on the user's behalf.

The plumbing for signing IS in place: `build-app.sh` signs and notarises when
given credentials (see "How to produce a signed build" below). What is missing
is the credentials themselves, in the keychain of whatever machine or CI
runner builds the release.

## What users do today

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

## What the signing setup needs (one-time)

To make the warning disappear entirely the release pipeline needs:

1. **An Apple Developer Program membership** (USD 99/year) for the org that
   ships hauksbee.
2. **A Developer ID Application certificate** issued from that account and
   installed in the CI keychain (as a base64 secret plus password, imported
   with `security import` into a temporary keychain on the runner).
3. **Code signing** of every Mach-O in the bundle, inside-out, with hardened
   runtime:

   ```sh
   codesign --force --options runtime --timestamp \
     --sign "Developer ID Application: <NAME> (<TEAMID>)" \
     Hauksbee.app/Contents/MacOS/hauksbee \
     Hauksbee.app/Contents/MacOS/hauksbee-ci
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

Until those pieces exist, the README and the download page state the caveat
plainly rather than pretending the app is blessed.
