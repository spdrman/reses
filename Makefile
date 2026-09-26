# gate, integration and demo run in Docker on the host RESES_NAS names; darwin fetches the macOS
# binary CI built. See CONTRIBUTING.md.
.PHONY: ci gate integration darwin install demo

ci: gate integration darwin
	@echo ""
	@echo "All CI checks passed."

gate:
	scripts/ci-docker.sh

integration:
	scripts/ci-docker.sh --integration

# The binary is the one CI built natively on macOS for this commit (the SDK can't be cross-linked
# for macOS from Linux). Checking it against the goldens runs the real macOS binary, which is the
# product under test rather than a toolchain. The replace test is
# #15: a rebuilt binary must still run after it has replaced one that already ran. Both run
# before dist/reses is replaced, so a build that fails either never becomes the installed one.
darwin:
	scripts/fetch-darwin.sh
	scripts/check-goldens.sh dist/reses-aarch64-apple-darwin
	tests/macos-replace-binary.sh dist/reses-aarch64-apple-darwin
	scripts/place-binary.sh dist/reses-aarch64-apple-darwin dist/reses

# ~/.local/bin/reses is a link into this checkout, so reinstalling only replaces dist/reses.
# An existing file or a link pointing somewhere else is left alone.
install: darwin
	@link="$$HOME/.local/bin/reses"; want="$(CURDIR)/dist/reses"; \
	if [ -L "$$link" ] && [ "$$(readlink "$$link")" = "$$want" ]; then \
		echo "$$link already points at $$want"; \
	elif [ -e "$$link" ] || [ -L "$$link" ]; then \
		echo "$$link exists and points elsewhere; move it aside, then rerun make install" >&2; exit 1; \
	else \
		mkdir -p "$$HOME/.local/bin" && ln -s "$$want" "$$link" && echo "linked $$link -> $$want"; \
	fi

# Regenerate docs/demo.gif from demo/demo.tape against a throwaway, seeded MinIO. The recording's
# text frames are checked before the GIF is replaced. See demo/record.sh.
demo:
	demo/record.sh
