.PHONY: build check

build:
	cd web && npm ci && npm run build
	cargo build --release --locked

check:
	cargo fmt --check
	cargo clippy --locked --all-targets -- -D warnings
	cargo test --locked
	shellcheck samba/*.sh deploy/*.sh tests/*.sh ci/*.sh
	shfmt -d -i 4 -ci samba/*.sh deploy/*.sh tests/*.sh ci/*.sh
	clang-format --dry-run --Werror samba/*.c
	ruff check --isolated tests/*.py
	ruff format --check --isolated tests/*.py
