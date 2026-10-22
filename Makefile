.PHONY: build
build:
	cargo build

.PHONY: format
format:
	cargo fmt --all $(FMT_FLAGS)

.PHONY: lint
lint:
	cargo clippy --all-targets -- -Dclippy::suspicious -Dclippy::style -Dclippy::nursery -Dclippy::pedantic -Dclippy::all \
	    -Dwarnings -Dlet_underscore_drop

.PHONY: test
test:
	cargo nextest run --all

.PHONY: test-events
test-events: build
	python3 scripts/test-event-save.py
