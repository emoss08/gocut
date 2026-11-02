#!/usr/bin/env bash
# Times Trenova's hot-reload build (cmd/api, run-watch flags) after representative edits,
# without gocut (baseline), with gocut in shadow mode, and with gocut active.
#
#   tms-run.sh <baseline|shadow|active> [scenario...]
#
# Scenarios: s4-end (exported func appended to a domain/shipment file), s4-mid (the same
# func inserted right after the imports, shifting every later line in that file),
# s2-body (a line added inside an unexported func in shipment.go; no API change, but it shifts
# every later line).
# Each scenario: warm build, edit + build (timed), restore + build (timed).
# NODWARF=1 compiles without DWARF (-dwarf=false); the link already strips it with -w.
set -euo pipefail

mode=${1:?mode}
shift
if (($#)); then scenarios=("$@"); else scenarios=(s4-end s4-mid s2-body); fi

TMS=${TMS:-$HOME/projects/Trenova/services/tms}
GOCUT=${GOCUT:-$HOME/projects/gocut/target/release/gocut}
DOMAIN=internal/modules/operations/domain/shipment
OUT=$(mktemp -d)
trap 'restore_all; rm -rf "$OUT"' EXIT

export GOFLAGS="-trimpath -tags=nomsgpack"
export GOWORK=${GOWORK:-}
case "$mode" in
baseline) export GOCACHE=$HOME/.cache/go-build-gocut-base; toolexec=() ;;
shadow | active) export GOCACHE=$HOME/.cache/go-build-gocut GOCUT_MODE=$mode GOCUT_DIR=$HOME/.cache/gocut-tms; toolexec=(-toolexec "$GOCUT") ;;
*) echo "mode must be baseline, shadow or active" >&2; exit 2 ;;
esac

NONCE=${NONCE:-$(date +%s%N | tail -c 9)}
declare -A backups=()

backup() {
	local f=$1
	if ! git -C "$TMS" diff --quiet -- "$f"; then
		echo "refusing to touch $f: it has uncommitted changes" >&2
		exit 1
	fi
	backups[$f]=$OUT/$(echo "$f" | tr / _).orig
	cp -p "$TMS/$f" "${backups[$f]}"
}

restore_all() {
	for f in "${!backups[@]}"; do
		cp -p "${backups[$f]}" "$TMS/$f.gocut-restore"
		mv -f "$TMS/$f.gocut-restore" "$TMS/$f"
	done
	backups=()
}

LIMIT_GB=${LIMIT_GB:-18}
export GOCUT_MAX_BYTES=${GOCUT_MAX_BYTES:-$((3 << 30))}

disk_guard() {
	local used
	used=$(du -sb "$GOCACHE" ${GOCUT_DIR:+"$GOCUT_DIR"} 2>/dev/null | awk '{s += $1} END {print int(s / 1073741824)}')
	if ((used >= LIMIT_GB)); then
		echo "stopping: measurement caches use ${used} GB (limit ${LIMIT_GB} GB)" >&2
		exit 3
	fi
	echo "caches ${used} GB" >&2
}

if [[ ${NODWARF:-} == 1 ]]; then
	gcflags=(-gcflags=all=-dwarf=false '-gcflags=github.com/emoss08/trenova/...=-N -l -dwarf=false')
else
	gcflags=('-gcflags=github.com/emoss08/trenova/...=-N -l')
fi

build() {
	local t0 t1
	t0=$(date +%s.%N)
	(cd "$TMS" && nice go build "${toolexec[@]}" -p "$(($(nproc) - 1))" \
		"${gcflags[@]}" -ldflags='-s -w' -o "$OUT/api" ./cmd/api)
	t1=$(date +%s.%N)
	disk_guard
	echo "$t1 - $t0" | bc
}

insert_after() {
	python3 -I - "$TMS/$1" "$2" "${@:3}" <<'EOF'
import re, sys
path, text, *patterns = sys.argv[1:]
src = open(path).read()
m = next((m for p in patterns if (m := re.search(p, src, re.M))), None)
if not m:
    sys.exit(f"no match for {patterns!r} in {path}")
open(path, "w").write(src[:m.end()] + text + src[m.end():])
EOF
}

edit() {
	local f=$DOMAIN/additionalcharge.go probe="func GocutProbe$NONCE() int { return 4 }"
	[[ $1 == s4-end ]] || f=$DOMAIN/shipment.go
	backup "$f"
	case "$1" in
	s4-end) printf '\n%s\n' "$probe" >>"$TMS/$f" ;;
	s4-mid) insert_after "$f" $'\n'"$probe"$'\n' '^import \((?:.|\n)*?^\)\n' '^import .*\n' ;;
	s2-body)
		insert_after "$f" $'\t_ = "gocut probe '"$NONCE"$'"\n' \
			'^func \(\w+ \*?\w+\) [a-z]\w*\([^)]*\)[^{\n]*\{\n' '^func [a-z]\w*\([^)]*\)[^{\n]*\{\n'
		;;
	*) echo "unknown scenario $1" >&2; exit 2 ;;
	esac
}

echo "mode=$mode GOCACHE=$GOCACHE NODWARF=${NODWARF:-0}"
echo "warm build: $(build)s"
for s in "${scenarios[@]}"; do
	[[ $mode == baseline ]] || "$GOCUT" clear-log
	touch "$OUT/stamp"
	edit "$s"
	e=$(build)
	restore_all
	r=$(build)
	find "$GOCACHE" -type f -newer "$OUT/stamp" -delete
	printf '%-8s edit %6.1fs   revert %6.1fs\n' "$s" "$e" "$r"
	if [[ $mode != baseline ]]; then
		"$GOCUT" stats | sed 's/^/    /'
	fi
done
