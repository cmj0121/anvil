SUBDIR :=
PREFIX ?= $(HOME)/.local
BINDIR ?= $(PREFIX)/bin

.PHONY: all clean test e2e run build install uninstall upgrade help $(SUBDIR)

all: $(SUBDIR) 		# default action
	@[ -f .git/hooks/pre-commit ] || pre-commit install --install-hooks
	@git config commit.template .git-commit-template

clean: $(SUBDIR)	# clean-up environment
	@find . -name '*.sw[po]' -delete

test:				# run test
	cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test

e2e:				# run the end-to-end suite (archlinux sshd container; skip without docker)
	@if [ -f tests/e2e.sh ]; then sh tests/e2e.sh; else echo skip; fi

run:				# run in the local environment
	cargo run -- $(ARGS)

build:				# build the binary/library
	cargo build --release --locked

install: build		# install anvil into $BINDIR (default ~/.local/bin)
	install -d "$(BINDIR)"
	install -m 0755 target/release/anvil "$(BINDIR)/anvil"
	@"$(BINDIR)/anvil" --version
	@found="$$(command -v anvil || true)"; [ "$$found" = "$(BINDIR)/anvil" ] || \
		echo "warning: anvil on PATH resolves to $${found:-nothing}, not $(BINDIR)/anvil" >&2

uninstall:			# remove anvil from $BINDIR
	rm -f "$(BINDIR)/anvil"

upgrade:			# upgrade all the necessary packages
	pre-commit autoupdate

help:				# show this message
	@printf "Usage: make [OPTION]\n"
	@printf "\n"
	@perl -nle 'print $$& if m{^[\w-]+:.*?#.*$$}' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?#"} {printf "    %-18s %s\n", $$1, $$2}'

$(SUBDIR):
	$(MAKE) -C $@ $(MAKECMDGOALS)
