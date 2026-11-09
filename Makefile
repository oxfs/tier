.PHONY: web build check scripts

web:
	cd web && npm ci --no-audit --no-fund && npm run build

build: web
	cargo build --release --locked

scripts:
	shellcheck samba/*.sh deploy/*.sh ci/*.sh tests/*.sh tests/smb/*.sh
	shfmt -d -i 4 -ci samba/*.sh deploy/*.sh ci/*.sh tests/*.sh tests/smb/*.sh
	ruff check tests/smb
	ruff format --check tests/smb

check: web scripts
	cd web && npm run fmt:check
	cd web && npm test
	clang-format --dry-run --Werror samba/*.c samba/*.h
	cargo fmt --check
	cargo clippy --locked --all-targets -- -D warnings
	cargo test --locked
