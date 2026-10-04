# Binary distribution and website deployment

This is the developer reference for GitHub binary distribution and the static
product site in `website/`. Preparing workflows does not publish a release,
install an app, change repository settings, or change DNS.

## Binary distribution

Every branch push and pull request to `main` runs [CI](../.github/workflows/ci.yml).
After Rust, minimum-toolchain, website, and native macOS GUI gates succeed,
the shared [native build workflow](../.github/workflows/build-binaries.yml)
builds on matching host architectures:

| Target | Native runner | Download | Acceptance status |
| --- | --- | --- | --- |
| macOS Apple Silicon | `macos-15` | `.tar.gz`: `MarkRust.app` and CLI | Native editor regression gates; distribution signing pending |
| macOS Intel | `macos-15-intel` | `.tar.gz`: `MarkRust.app` and CLI | Native Rust tests and executable smoke; hardware GUI QA pending |
| Linux x86_64 | `ubuntu-22.04` | `.tar.gz`: executable, desktop entry, icon | Experimental distribution; desktop/clipboard/IME QA pending |
| Windows x86_64 | `windows-2022` | `markrust-windows-x86_64-experimental.zip` | Experimental; GUI and recovery ACL/locking acceptance pending |

GPUI revision `8166e3d7b8b42d8aaf4d4dee7fcd25ab4ec65105` has real native
Windows, Linux, and macOS backends. Its Windows release build needs the Windows
SDK's `fxc.exe` shader compiler; macOS needs Xcode's Metal compiler. The matrix
checks those prerequisites and runs the Rust suite on every native platform.
Green CLI/build tests do not establish native GUI, IME, clipboard, accessibility,
or recovery-security acceptance. In particular, Unix-specific recovery ownership,
permissions, and locking need equivalent native Windows verification/implementation.

Successful runs expose commit-specific archives under **Actions → CI → run →
Artifacts**, retained for 14 days (subject to repository limits). Each archive
includes licenses and `BUILD-INFO.json` with compiled UTC build time, version,
checkout commit, target, and signing status. Packaging executes the exact staged
target binary with `--version` and `--build-info` and creates a SHA-256 sidecar.
Linux includes its `ldd` runtime-library report; it needs a compatible X11/Wayland
desktop, Vulkan graphics, and those system libraries. It is not a self-contained
AppImage. Windows may require the Microsoft Visual C++ 2015–2022 x64 runtime.

Push builds are development artifacts, not mutable stable releases. An explicitly
chosen `vMAJOR.MINOR.PATCH` tag, matching the workspace version, invokes
[Release](../.github/workflows/release.yml). Publication waits for all existing
verification gates, all four native archives, and checksum verification.
Matching prerelease tags are marked as GitHub prereleases. Optional immutable
per-commit prereleases are not enabled by default. The publisher refuses to
overwrite an existing version-tag release; publish a new version after review.

External actions in binary CI/release workflows are pinned to full commit SHAs.
Build jobs have read-only repository access; checkout credentials are not retained
and branch/PR builds receive no signing secrets. Only the final tag publisher
receives `contents: write`; it downloads artifacts from the same verified run
without checking out or executing application code. Website/DNS approval remains
separate and unchanged.

### Repository prerequisites and decisions

Read-only repository checks on 2026-10-04 found public repository
`alexey-a-abramov/markrust`, default branch `main`, Actions enabled, and default
workflow permissions set to read. The publisher's explicit `contents: write`
uses the built-in `GITHUB_TOKEN`; unsigned builds/releases need no personal
access token. Confirm the first remote run: runner labels and policies can change.

The first remote matrix and tagged release are being validated. Local tests do
not establish remote publication or Windows/Linux desktop acceptance. Signing
credentials, repository settings and DNS remain unchanged.

| Question | Recommendation | User decision | Notes (optional) |
| --- | --- | --- | --- |
| Publish the first public version tag after remote gates and archive review? | Install the `0.8.0` updater seed locally, then publish `v0.8.1` for an actual update test. | Approved by the user on 2026-10-04: install locally, publish a new version and test GitHub release updates. | Publication remains gated on verification; unrelated local settings are excluded. |
| Sign/notarize macOS downloads? | Use Apple Developer ID and notarization for normal public installation. Current bundles are only ad-hoc signed; Gatekeeper may block them. | _[Enter your decision here]_ | _[Enter comments or conditions here]_ |
| Sign Windows downloads? | Obtain an Authenticode identity after native/recovery acceptance passes. Current downloads are unsigned; SmartScreen may warn. | _[Enter your decision here]_ | _[Enter comments or conditions here]_ |
| Publish immutable prereleases for successful `main` commits? | Start with 14-day Actions artifacts plus version-tag Releases; add per-SHA public links only if useful. | _[Enter your decision here]_ | _[Enter comments or conditions here]_ |

Signing needs the relevant developer account/certificate and secure CI secret
setup, not credentials pasted into chat. No signing secrets are needed to start
the unsigned matrix. Platform installers, Linux ARM64, Windows/Linux in-app updates, and
package-manager publication remain separate work.

### In-app macOS updates

MarkRust `0.8.0+` checks the fixed public repository's latest stable GitHub
Release after launch and approximately every 24 hours. The application menu
provides **Check for Updates**, **Automatically Check for Updates**, and
**GitHub Repository**. The automatic-check preference is persisted and applies
to all windows. Checks run in the background; automatic checks with no update
or a network error do not interrupt editing. Drafts, document text and file
paths are not sent to GitHub.

Updates require explicit **Download Update** and **Restart and Update** actions.
No background installation or unsolicited restart occurs. Only a numerically
newer stable version for the current macOS architecture is eligible. Downloads
are constrained to this repository's GitHub assets and bounded in time/size;
the archive, exact checksum filename, optional GitHub asset digest, bundle
identity/version, native executable architecture and strict code signature are
checked before preparation. Extraction rejects traversal, links, special files
and oversized archives. Staging is owner-only and never executes downloaded code.

SHA-256 and an ad-hoc signature detect corruption and inconsistent bundles, not
an independent publisher identity. The current trust boundary is the fixed
GitHub repository over HTTPS. Developer ID/notarization and independently signed
update metadata remain pending approval; these downloads are not equivalent to
a notarized public installer. Shared multi-user installation is not certified.

Restart is blocked while a dialog, image inspector or text composition is open.
All registered windows must checkpoint their private buffers successfully first;
failure leaves the application and documents open. A helper copied from the
current trusted executable waits for that exact process to exit. Input is frozen
during handoff; if termination is vetoed, the helper is cancelled before editing
resumes. The helper rechecks pinned target/stage identities, atomically swaps
the bundle on the same volume, retains `Previous-MarkRust.app`, and writes an
owner-only `receipt.json` in a unique `.MarkRust-update-*` installation directory.
Promotion or launch-request failures restore the old bundle. LaunchServices
acceptance alone is not proof of successful application startup: verify the new
process, build identity and recovered draft during the actual update test.

Only macOS Apple Silicon and Intel support in-app replacement. Other platforms
offer the repository link and report that installation is unsupported. Ordinary
Save remains byte-preserving; update checks never normalize or publish documents.

### Local developer verification

```bash
python3 -m unittest discover -s scripts/tests -v
MARKRUST_RELEASE_TAG=v0.8.1 python3 scripts/package-release.py --check-tag
bash scripts/release.sh
```

The 16 synthetic packaging/workflow contracts use no installed apps or personal
recovery data. They verify tag/version/architecture guards, pinned actions,
archive contents, checksum output, missing-library rejection, and experimental
Windows naming; they are not cross-platform build proof. `package-release.py`
requires Python 3.11+ and an already-built native executable. It refuses to
overwrite an archive and never installs, opens the GUI, tags, or publishes.
Review the first remote run's build metadata and archives before a public tag.

The local macOS ARM64 `0.7.0` package was built and extracted on 2026-10-04.
Its CLI/app identity reports `2026-10-04T14:39:57Z`; strict ad-hoc signature,
numeric bundle versions, executable permissions and archive checksum checks
passed. This is not an installed or public release. `--commit` validates a
caller-supplied checkout SHA, not embedded compiled provenance: a package from
a dirty working tree must not be advertised as the exact committed source.
The native CI matrix uses fresh builds from its own checkout. See the
[local implementation evidence](notepad-experience.md#local-verification-2026-10-04).

## Website deployment and custom domain

## Current state

As checked on 2026-09-14, `markrust.org` did not resolve to an A, AAAA, or
CNAME record from the release workstation. The repository now includes a
GitHub Pages workflow at
[`website-pages.yml`](../.github/workflows/website-pages.yml), which builds
the Astro site from `main` and deploys its `website/dist` artifact.

The workflow alone cannot activate GitHub Pages or change DNS. Those actions
require repository-admin access and control of the `markrust.org` DNS zone.
Confirm or establish domain registration with a registrar first; a DNS lookup
alone cannot prove who controls the domain.

## One-time GitHub Pages setup

1. Confirm that the registrar account controls `markrust.org` and its DNS
   zone. If it is not registered, register it before continuing.
2. [Verify the custom domain in GitHub Pages](https://docs.github.com/en/pages/configuring-a-custom-domain-for-your-github-pages-site/verifying-your-custom-domain-for-github-pages)
   from the account's **Settings → Pages** page (not repository settings).
   Add the exact `_github-pages-challenge-…` TXT record GitHub generates to
   the DNS zone, and leave it in place.
3. In the repository, open **Settings → Pages**, select **GitHub Actions** as
   the publishing source, then set the custom domain to `markrust.org`.
4. In **Settings → Secrets and variables → Actions → Variables**, set
   `PAGES_CUSTOM_DOMAIN_READY` to `true`. This is a deliberate deployment
   interlock: it keeps the workflow from publishing root-absolute links to the
   default project URL before the custom domain is configured.
5. Only after steps 1–4, push to `main` or use **Run workflow** from `main`.
   Then add the DNS records below.
6. Enable HTTPS enforcement after GitHub has provisioned the certificate.

## DNS records

At the DNS provider, add all four IPv4 records for the apex domain. Add the
IPv6 records if the provider supports them, and point `www` at the account's
GitHub Pages hostname so GitHub can redirect it to the canonical apex domain.

| Type | Host | Value |
| --- | --- | --- |
| A | `@` | `185.199.108.153` |
| A | `@` | `185.199.109.153` |
| A | `@` | `185.199.110.153` |
| A | `@` | `185.199.111.153` |
| AAAA | `@` | `2606:50c0:8000::153` |
| AAAA | `@` | `2606:50c0:8001::153` |
| AAAA | `@` | `2606:50c0:8002::153` |
| AAAA | `@` | `2606:50c0:8003::153` |
| CNAME | `www` | `alexey-a-abramov.github.io` |

Review existing records before changing them. Replace only web-hosting records
that conflict with the Pages apex records; do not delete MX, TXT, mail, or
other unrelated service records. Do not use wildcard DNS records with GitHub
Pages. DNS propagation and certificate provisioning can take up to 24 hours.

## Verification

```bash
dig markrust.org A +noall +answer
dig markrust.org AAAA +noall +answer
dig www.markrust.org CNAME +noall +answer
curl -I https://markrust.org
```

The `A` and `AAAA` results should match the table, `www` should point to the
GitHub Pages hostname, and HTTPS should return a successful response without a
certificate warning.

## Related

- [Engineering documentation index](README.md) — documentation hub
- [Native GUI testing](gui-testing.md) — interaction/geometry gates and pixel-baseline limitations
- [GitHub workflow artifacts](https://docs.github.com/en/actions/tutorials/store-and-share-data) — download and retention behavior
- [GitHub-hosted runner reference](https://docs.github.com/en/actions/reference/runners/github-hosted-runners) — native architectures and labels
- [Pinned GPUI platform implementation](https://github.com/zed-industries/zed/blob/8166e3d7b8b42d8aaf4d4dee7fcd25ab4ec65105/crates/gpui_platform/src/gpui_platform.rs) — native backend constructors
- [Project roadmap](../ROADMAP.md) — release gates and priorities
- [GitHub Pages custom domains](https://docs.github.com/en/pages/configuring-a-custom-domain-for-your-github-pages-site/managing-a-custom-domain-for-your-github-pages-site) — current provider guidance
