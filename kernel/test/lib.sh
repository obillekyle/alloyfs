# Shell assertions for in-guest test cases. Sourced, not executed.
# Every case script must `. /tests/lib.sh` and end with `summary`.

ds_fails=0
ds_checks=0

ok() {   # ok "what" — record a pass
	ds_checks=$((ds_checks + 1))
	echo "  ok: $1"
}

fail() { # fail "what" — record a failure (does not abort; we want the full picture)
	ds_checks=$((ds_checks + 1))
	ds_fails=$((ds_fails + 1))
	echo "  FAIL: $1"
}

check() { # check "what" <condition-cmd...>
	desc="$1"; shift
	if "$@"; then ok "$desc"; else fail "$desc"; fi
}

eq() {   # eq "what" expected actual
	if [ "$2" = "$3" ]; then
		ok "$1"
	else
		fail "$1 (expected [$2], got [$3])"
	fi
}

contains() { # contains "what" haystack needle
	case "$2" in
	*"$3"*) ok "$1" ;;
	*)      fail "$1 (no [$3] in [$2])" ;;
	esac
}

summary() {
	echo "  -- $((ds_checks - ds_fails))/$ds_checks checks passed"
	[ "$ds_fails" -eq 0 ]
}
