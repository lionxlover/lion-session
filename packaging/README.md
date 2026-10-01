# lion-session distro packaging

Three in-repo packaging specs, maintained next to the source they
package (drift between them is a bug):

| Family | File | Build command |
|---|---|---|
| Arch / pacman / AUR | `PKGBUILD` | `makepkg -si` |
| Debian / Ubuntu | `debian/` | `dpkg-buildpackage -us -uc` |
| Fedora / RHEL / SUSE | `rpm/lion-session.spec` | `rpmbuild -ba rpm/lion-session.spec` |

All three build from the same `--locked` graph, run the full test
suite (unit + mock-peer integration + fuzz harness) at package-build
time, and install the same file set: the binary, the two user units
(the `lion-session.target` app group and the activation/restart
`lion-session.service`), the D-Bus activation service file, the
documented `/etc/lionos/session.toml`, and the operator docs.

`%config(noreplace)` / the Makefile's install rules keep the sysadmin's
live config on upgrade; the shipped file is a fully commented default.
