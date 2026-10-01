# lion-session Makefile — build / test / check / install / dist.
#
# Intended users: distro packagers, sysadmins, CI (which runs exactly
# `make check test`).

DESTDIR ?=
PREFIX  ?= /usr
BINDIR  := $(PREFIX)/bin
UNITDIR_USER := $(PREFIX)/lib/systemd/user
DBUSDIR := $(PREFIX)/share/dbus-1/services
DOCDIR  := $(PREFIX)/share/doc/lion-session
SYSCONF := /etc
VERSION := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)

.PHONY: default release test check live install uninstall dist clean

default:
	cargo build --locked

release:
	cargo build --locked --release

test:
	cargo test --locked --all-targets

check:
	cargo fmt --all -- --check
	cargo clippy --all-targets --locked -- -D warnings
	cargo test --locked --all-targets

live: release
	bash scripts/live_session_test.sh

install: release
	install -Dm755 target/release/lion-session $(DESTDIR)$(BINDIR)/lion-session
	install -Dm644 systemd/lion-session.target $(DESTDIR)$(UNITDIR_USER)/lion-session.target
	install -Dm644 systemd/lion-session.service $(DESTDIR)$(UNITDIR_USER)/lion-session.service
	install -Dm644 dbus-1/services/os.lionos.Session.service $(DESTDIR)$(DBUSDIR)/os.lionos.Session.service
	install -Dm644 etc/lionos/session.toml $(DESTDIR)$(SYSCONF)/lionos/session.toml
	install -Dm644 README.md $(DESTDIR)$(DOCDIR)/README.md
	install -Dm644 CHANGELOG.md $(DESTDIR)$(DOCDIR)/CHANGELOG.md
	install -Dm644 STABILITY.md $(DESTDIR)$(DOCDIR)/STABILITY.md
	install -Dm644 SECURITY.md $(DESTDIR)$(DOCDIR)/SECURITY.md
	@echo "installed lion-session $(VERSION) -> DESTDIR=$(DESTDIR)"

uninstall:
	rm -f $(DESTDIR)$(BINDIR)/lion-session \
	      $(DESTDIR)$(UNITDIR_USER)/lion-session.target \
	      $(DESTDIR)$(UNITDIR_USER)/lion-session.service \
	      $(DESTDIR)$(DBUSDIR)/os.lionos.Session.service
	rm -rf $(DESTDIR)$(DOCDIR)
	# /etc/lionos/session.toml and user configs are intentionally kept.

dist:
	git archive --format=tar.gz --prefix=lion-session-$(VERSION)/ \
		-o lion-session-$(VERSION).tar.gz HEAD

clean:
	cargo clean
	rm -f lion-session-$(VERSION).tar.gz
