.PHONY: web build check

web:
	cd web && npm ci --no-audit --no-fund && npm run build

build: web
	cargo build --release --locked

check: web
	cd web && npm run fmt:check
	cd web && npm test
	clang-format --dry-run --Werror samba/*.c samba/*.h
	cargo fmt --check
	cargo clippy --locked --all-targets -- -D warnings
	cargo test --locked
