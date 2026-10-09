# remoteadb build conventions

CARGO ?= cargo
PREFIX ?= /usr/local

.PHONY: build release test clippy fmt check-windows install clean

build:
	$(CARGO) build

release:
	$(CARGO) build --release

test:
	$(CARGO) test

clippy:
	$(CARGO) clippy --all-targets

fmt:
	$(CARGO) fmt --all -- --config unstable_features=true --config imports_granularity=Crate,group_imports=StdExternalCrate,reorder_imports=true

# Type-check the Windows service backend without a Windows machine.
check-windows:
	$(CARGO) check --target x86_64-pc-windows-gnu

install: release
	install -Dm755 target/release/remoteadb $(DESTDIR)$(PREFIX)/bin/remoteadb

clean:
	$(CARGO) clean
