# Dependency security review

Last reviewed: 2026-10-22, using cargo-audit 0.22.2 and the current
RustSec database. The workspace lockfile contains one vulnerability and
six informational warnings. `cargo update --dry-run` found no compatible
updates; none of these findings can currently be fixed by a compatible
lockfile refresh.

Run the full, unfiltered report with `cargo audit`. It intentionally
continues to report the RSA vulnerability: there is no global ignore
configuration. CI checks new vulnerabilities and unsoundness with the two
specific exceptions below, backed by code and dependency-graph checks.
Maintenance warnings remain visible.

## RSA: RUSTSEC-2023-0071

`ferry -> russh 0.63.3 -> rsa 0.10.0-rc.18` (also through `ssh-key`).
[The advisory](https://rustsec.org/advisories/RUSTSEC-2023-0071) describes
private-key recovery through network-observable timing and lists no fixed
release, including for the release candidate already in this lockfile.

Ferry generates ECDSA identities. It uses russh only as an SSH/SFTP
client, and needs RSA to verify the public host keys of older Android
peers; disabling the feature would break that interoperability. Public
key verification does not involve a local RSA secret. TLS is handled by
rustls/ring, independently of the RustCrypto `rsa` crate.

As defense in depth, `core::payload::ssh_authentication_key` explicitly
rejects RSA private keys before SSH authentication, including keys from
stored identities. Regression tests exercise a real RSA private-key
fixture and a generated ECDSA key; the browse tests separately preserve
RSA public host-key matching. An existing RSA identity is not regenerated
or unpaired: file browsing fails with `files_failed`.

CI excludes this advisory only on that basis. Revisit the exception if
Ferry adds RSA private-key operations, an SSH server, another use of
`rsa`, or changes the SSH backend. Remove it when upstream offers a fixed
version or RSA public verification can use another backend. The crate
itself remains vulnerable; these restrictions reduce Ferry's exposure.

## Unselected GPU dependency: RUSTSEC-2026-0253

`iced_wgpu -> cryoglyph 0.1.0 -> lru 0.16.4` is recorded in the lockfile
but absent from the selected workspace dependency graph on all platforms.
Ferry explicitly disables iced's default features and renders with
tiny-skia. Cargo records optional dependencies in its lockfile even when
they are not compiled.

[The advisory](https://rustsec.org/advisories/RUSTSEC-2026-0253) describes
use-after-free/double-free after a key's destructor panics in `pop()` and
execution continues after catching the panic. The fix is `lru >= 0.18.2`,
outside cryoglyph's compatible dependency range. We have not patched or
vendored this unused renderer.

CI checks `cargo tree --workspace --target all -e normal,build,dev` and
fails if **any** `lru` becomes selected, before excluding this advisory
from `cargo audit --deny unsound`. Enabling wgpu, including through tests
or a new dependency, requires revisiting this exception and upgrading or
patching the renderer's dependency first. Remove the exception when the
vulnerable crate leaves the lockfile or its parent accepts a fixed version.

## Unmaintained transitive dependencies

| Finding | Dependency path and exposure | Follow-up |
| --- | --- | --- |
| [RUSTSEC-2026-0374](https://rustsec.org/advisories/RUSTSEC-2026-0374): `basic-toml 0.1.10` | `i18n-config`, through translation proc macros; parses repository configuration at build time. | Adopt an i18n release that replaces it with maintained `toml`. |
| [RUSTSEC-2026-0192](https://rustsec.org/advisories/RUSTSEC-2026-0192): `ttf-parser 0.25.1` | iced's font stack (`fontdb`, `owned_ttf_parser`) and icon proc macros; bundled and system fonts, with no feature to import remote fonts. | Follow the font stack's migration to a maintained parser. iced 0.14 is pinned; a toolkit migration needs separate compatibility work. |
| [RUSTSEC-2026-0370](https://rustsec.org/advisories/RUSTSEC-2026-0370), [0371](https://rustsec.org/advisories/RUSTSEC-2026-0371), [0372](https://rustsec.org/advisories/RUSTSEC-2026-0372): `zvariant 5.15.1`, `zbus_names 4.3.5`, `zvariant_derive 5.15.1` | zbus 5, directly and through `ksni`, `mundy` and `opener`; Linux desktop D-Bus integration. | These crates merged into zbus 6. Coordinate the migration with their upstream consumers; changing only Ferry's direct zbus would leave the old transitive copies. |

These are maintenance advisories, not reported exploitable vulnerabilities.
There are no compatible replacements available in the current graph.
Keep them visible in CI and continue reviewing Dependabot's weekly
updates. Do not use a broad advisory ignore or force incompatible versions
through a dependency patch just to make the report empty.
