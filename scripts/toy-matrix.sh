#!/usr/bin/env bash
#
# toy-matrix.sh: run gocut over tiny throwaway Go modules, one edit to package ex/a per scenario,
# in two flag modes, and check each first-party compile's decision against an expectation.
#
# Run with --help for usage.
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
GOCUT=${GOCUT:-"$script_dir/../target/release/gocut"}
GOCUT_MODE=${GOCUT_MODE:-shadow}
MODES=${MODES:-"default nl"}
KEEP=${KEEP:-0}
TOY_GO_P=${TOY_GO_P:-}

usage() {
	cat <<'EOF'
usage: scripts/toy-matrix.sh [--list] [scenario ...]

Builds module "ex" (packages ex/a, ex/b, ex/p, ex/q and ex/cmd) under a temp dir. For each
scenario and flag mode it: builds once with -toolexec gocut (fresh GOCUT_DIR and GOCACHE),
applies one edit to ex/a, clears the gocut log, rebuilds with gocut, builds the same tree
without gocut, compares the two binaries' output, and checks every first-party compile.

environment:
  GOCUT=path          gocut binary (default: target/release/gocut next to this script)
  GOCUT_MODE=mode     shadow (default) | active | record
  MODES="default nl"  flag modes: "default" (no extra flags), "nl" (-gcflags=ex/...=-N -l),
                      "nld" (-gcflags=ex/...=-N -l -dwarf=false)
  TOY_GO_P=N          pass -p N to go build (less parallelism, less memory)
  KEEP=1              keep the work dir even when everything passes (kept on failure anyway)

expectation tokens (alternatives joined with |):
  reuse        decision reuse and shadow match (in active mode: decision reuse)
  blocked     blocked for any reason; blocked~TEXT also requires TEXT in the reason
  no-record    no record for this compile key (expected for ex/a: its source changed)
  skip         go did not recompile the package at all
  observe      no expectation; the row is only reported
A shadow mismatch, a compile failure, a gocut internal error (patch/record/prepare failed)
and a gocut-built binary whose output differs from the plain build always count as failures.
EOF
}

die() {
	printf 'toy-matrix: %s\n' "$*" >&2
	exit 2
}

SCENARIOS=(
	add-func-end
	add-func-middle
	add-func-newfile
	body-uncalled
	body-called-inline
	body-noninline-called
	field-unused
	field-pused
	method-on-used-type
	method-on-unused-type
	add-unexported-func
	add-const-var-type
	generics-unrelated
	add-init
	comment-only
	reexport-unrelated
	iface-method-body
	embedded-unrelated-field
)

declare -A DESC=(
	[add-func-end]="append an unused exported func to a.go"
	[add-func-middle]="insert an unused exported func near the top of a.go (shifts every later decl)"
	[add-func-newfile]="add an unused exported func in a new file a2.go"
	[body-uncalled]="change bodies of Small (inlinable) and Big (noinline); p calls both, q neither"
	[body-called-inline]="change the body of QSmall (inlinable), which q calls"
	[body-noninline-called]="change the body of QBig (noinline), which q calls; export data unchanged"
	[field-unused]="add a field to struct Unused, which nobody uses"
	[field-pused]="add a field to struct PUsed, which p uses"
	[method-on-used-type]="add method Reset to Counter, which q uses (method set changes)"
	[method-on-unused-type]="add a method to Unused, which nobody uses"
	[add-unexported-func]="append an unexported func"
	[add-const-var-type]="append an exported const, var and type"
	[generics-unrelated]="append generic func Last; q instantiates Box[int] and Pair[string]"
	[add-init]="add func init() to a"
	[comment-only]="edit a doc comment, same line count"
	[reexport-unrelated]="append an unrelated func; b re-exports a.T in its API (b.Get() a.T)"
	[iface-method-body]="change the body of Counter.Inc, which q calls through an interface q defines"
	[embedded-unrelated-field]="add a field to T (b and p use it); q embeds a.Base in its own struct"
)

replace_once() {
	python3 -I - "$1" "$2" "$3" <<'PY'
import sys

path, old, new = sys.argv[1], sys.argv[2], sys.argv[3]
with open(path, encoding="utf-8") as f:
    src = f.read()
n = src.count(old)
if n != 1:
    sys.exit(f"edit: expected exactly one {old!r} in {path}, found {n}")
with open(path, "w", encoding="utf-8") as f:
    f.write(src.replace(old, new))
PY
}

append_a() {
	printf '\n%s\n' "$2" >>"$1/a/a.go"
}

write_tree() {
	local src=$1
	mkdir -p -- "$src/a" "$src/b" "$src/p" "$src/q" "$src/cmd"
	printf 'module ex\n\ngo %s\n' "$go_line" >"$src/go.mod"

	cat >"$src/a/a.go" <<'EOF'
package a

// Version is the toy version.
const Version = 3

var Greeting = "hi"

type T struct {
	X int
	Y int
}

func (t T) Sum() int { return t.X + t.Y }

type Unused struct{ A int }

type PUsed struct{ N int }

func Small(x int) int { return x + 1 }

func QSmall(x int) int { return x * 2 }

//go:noinline
func Big(n int) int {
	s := 0
	for i := 0; i < n; i++ {
		s += i * 3
	}
	return s + 11
}

//go:noinline
func QBig(n int) int {
	t := 1
	for k := 0; k < n; k++ {
		t = t*17 + k
	}
	return t % 1000003
}

type Counter struct{ n int }

func NewCounter() *Counter { return &Counter{} }

func (c *Counter) Inc() { c.n += 1 }

func (c *Counter) Get() int { return c.n }

type Base struct{ ID int }

func (b Base) Name() string { return "base" }

type Box[E any] struct{ Val E }

func (b Box[E]) Get() E { return b.Val }

func Pair[E any](x, y E) [2]E { return [2]E{x, y} }

func Map[E, U any](xs []E, f func(E) U) []U {
	out := make([]U, 0, len(xs))
	for _, x := range xs {
		out = append(out, f(x))
	}
	return out
}
EOF

	cat >"$src/b/b.go" <<'EOF'
package b

import "ex/a"

func Get() a.T { return a.T{X: 1, Y: 2} }

func Describe(t a.T) int { return t.Sum() * 10 }
EOF

	cat >"$src/p/p.go" <<'EOF'
package p

import (
	"ex/a"
	"ex/b"
)

func Run() int {
	t := b.Get()
	u := a.PUsed{N: 4}
	return b.Describe(t) + u.N + a.Small(5) + a.Big(10) + t.Sum()
}
EOF

	cat >"$src/q/q.go" <<'EOF'
package q

import "ex/a"

type Incer interface {
	Inc()
	Get() int
}

type Item struct {
	a.Base
	Qty int
}

var _ Incer = (*a.Counter)(nil)

func Run() int {
	var c Incer = a.NewCounter()
	c.Inc()
	c.Inc()
	it := Item{Base: a.Base{ID: 7}, Qty: 3}
	bx := a.Box[int]{Val: 9}
	pr := a.Pair("x", "yz")
	n := len(it.Name()) + it.ID + it.Qty + bx.Get() + len(pr[0]) + len(pr[1])
	return n + c.Get() + a.QSmall(4) + a.QBig(6) + len(a.Greeting)
}
EOF

	cat >"$src/cmd/main.go" <<'EOF'
package main

import (
	"ex/p"
	"ex/q"
)

func main() {
	println("p.Run", p.Run())
	println("q.Run", q.Run())
}
EOF
}

edit_add_func_end() {
	append_a "$1" 'func AddedEnd() int { return 42 }'
}

edit_add_func_middle() {
	replace_once "$1/a/a.go" $'const Version = 3\n' \
		$'const Version = 3\n\nfunc AddedMiddle() int { return 43 }\n'
}

edit_add_func_newfile() {
	cat >"$1/a/a2.go" <<'EOF'
package a

func AddedFile() int { return 44 }
EOF
}

edit_body_uncalled() {
	replace_once "$1/a/a.go" 'return x + 1' 'return x + 2' || return 1
	replace_once "$1/a/a.go" 'return s + 11' 'return s + 13'
}

edit_body_called_inline() {
	replace_once "$1/a/a.go" 'return x * 2' 'return x * 3'
}

edit_body_noninline_called() {
	replace_once "$1/a/a.go" 't = t*17 + k' 't = t*19 + k'
}

edit_field_unused() {
	replace_once "$1/a/a.go" 'type Unused struct{ A int }' 'type Unused struct{ A int; Extra int }'
}

edit_field_pused() {
	replace_once "$1/a/a.go" 'type PUsed struct{ N int }' 'type PUsed struct{ N int; Extra int }'
}

edit_method_on_used_type() {
	append_a "$1" 'func (c *Counter) Reset() { c.n = 0 }'
}

edit_method_on_unused_type() {
	append_a "$1" 'func (u Unused) Twice() int { return u.A * 2 }'
}

edit_add_unexported_func() {
	append_a "$1" 'func helper() int { return 7 }'
}

edit_add_const_var_type() {
	cat >>"$1/a/a.go" <<'EOF'

const NewConst = 5

var NewVar = 6

type NewType struct{ V int }
EOF
}

edit_generics_unrelated() {
	append_a "$1" 'func Last[E any](xs []E) E { return xs[len(xs)-1] }'
}

edit_add_init() {
	append_a "$1" 'func init() { Greeting = "hello" }'
}

edit_comment_only() {
	replace_once "$1/a/a.go" '// Version is the toy version.' '// Version is the toy version, edited.'
}

edit_reexport_unrelated() {
	append_a "$1" 'func Other() string { return "other" }'
}

edit_iface_method_body() {
	replace_once "$1/a/a.go" 'c.n += 1' 'c.n += 2'
}

edit_embedded_unrelated_field() {
	replace_once "$1/a/a.go" $'Y int\n}' $'Y int; Z int\n}'
}

want() {
	printf '%s %s\n' "$1" "$2"
}

want3() {
	want ex/b "$1"
	want ex/p "$1"
	want ex/q "$1"
}

exp_add_func_end() { want3 reuse; }

exp_add_func_middle() { want3 observe; }

exp_add_func_newfile() { want3 reuse; }

exp_body_uncalled() {
	if [[ $1 == default ]]; then
		want ex/b reuse
		want ex/p 'blocked~ex/a.Small'
		want ex/q reuse
	else
		want ex/b reuse
		want ex/p 'reuse|blocked'
		want ex/q reuse
	fi
}

exp_body_called_inline() {
	if [[ $1 == default ]]; then
		want ex/b reuse
		want ex/p reuse
		want ex/q 'blocked~ex/a.QSmall'
	else
		want ex/b reuse
		want ex/p reuse
		want ex/q observe
	fi
}

exp_body_noninline_called() { want3 reuse; }

exp_field_unused() { want3 reuse; }

exp_field_pused() {
	want ex/b reuse
	want ex/p 'blocked~ex/a.PUsed'
	want ex/q reuse
}

exp_method_on_used_type() {
	want ex/b reuse
	want ex/p reuse
	want ex/q 'blocked~Counter'
}

exp_method_on_unused_type() { want3 reuse; }

exp_add_unexported_func() { want3 reuse; }

exp_add_const_var_type() { want3 reuse; }

exp_generics_unrelated() { want3 reuse; }

exp_add_init() { want3 'blocked~init changed'; }

exp_comment_only() { want3 'skip|reuse'; }

exp_reexport_unrelated() { want3 reuse; }

exp_iface_method_body() {
	if [[ $1 == default ]]; then
		want ex/b reuse
		want ex/p reuse
		want ex/q 'blocked~Counter'
	else
		want3 reuse
	fi
}

exp_embedded_unrelated_field() {
	want ex/b 'blocked~ex/a.T'
	want ex/p blocked
	want ex/q reuse
}

parse_events() {
	python3 -I - "$1" <<'PY'
import json
import sys

path = sys.argv[1]
try:
    with open(path, encoding="utf-8") as f:
        data = f.read()
except FileNotFoundError:
    data = ""

decoder = json.JSONDecoder()
events = []
bad = 0
for line in data.splitlines():
    i, n = 0, len(line)
    while i < n:
        while i < n and line[i].isspace():
            i += 1
        if i >= n:
            break
        try:
            obj, i = decoder.raw_decode(line, i)
        except ValueError:
            bad += 1
            break
        if isinstance(obj, dict):
            events.append(obj)

if bad:
    print(f"toy-matrix: {path}: {bad} unparsable line(s)", file=sys.stderr)


def first_party(pkg):
    return pkg == "main" or pkg.startswith("ex/")


def clean(value):
    s = " ".join(str(value).split())
    return s if s else "-"


last = {}
count = {}
for ev in events:
    pkg = str(ev.get("pkg", ""))
    if not first_party(pkg):
        continue
    last[pkg] = ev
    count[pkg] = count.get(pkg, 0) + 1

for pkg, ev in last.items():
    reason = str(ev.get("reason", ""))
    if count[pkg] > 1:
        reason = f"[compiled {count[pkg]}x] {reason}"
    fields = (
        pkg,
        ev.get("decision", ""),
        ev.get("shadow", ""),
        ev.get("changed_deps", 0),
        reason,
    )
    print("\t".join(clean(x) for x in fields))
PY
}

run_go() {
	local src=$1 log=$2
	shift 2
	(cd -- "$src" && env GOFLAGS=-trimpath GOWORK=off GOTOOLCHAIN=local "$@") >"$log" 2>&1
}

build_gocut() {
	local src=$1 cache=$2 gdir=$3 out=$4 log=$5
	shift 5
	run_go "$src" "$log" GOCACHE="$cache" GOCUT_DIR="$gdir" GOCUT_MODE="$GOCUT_MODE" \
		go build -toolexec "$GOCUT" "$@" -o "$out" ./cmd
}

build_plain() {
	local src=$1 out=$2 log=$3
	shift 3
	run_go "$src" "$log" GOCACHE="$plain_cache" go build "$@" -o "$out" ./cmd
}

spec_matches() {
	local spec=$1 present=$2 d=$3 s=$4 r=$5 alt
	local -a alts=()
	IFS='|' read -r -a alts <<<"$spec"
	for alt in "${alts[@]}"; do
		case $alt in
		observe)
			return 0
			;;
		skip)
			if [[ $present == 0 ]]; then return 0; fi
			;;
		reuse)
			if [[ $present == 1 && $d == reuse && ($s == match || $GOCUT_MODE == active) ]]; then
				return 0
			fi
			;;
		blocked~*)
			if [[ $present == 1 && $d == blocked && $r == *"${alt#blocked~}"* ]]; then
				return 0
			fi
			;;
		*)
			if [[ $present == 1 && $d == "$alt" ]]; then return 0; fi
			;;
		esac
	done
	return 1
}

row_fmt='%-25s %-7s %-6s %-12s %-12s %-3s %-20s %-4s %s\n'
reason_width=70
internal_re='patch failed|patch panicked|record failed|prepare failed'

# shellcheck disable=SC2059
row() { printf "$row_fmt" "$@"; }

n_rows=0
n_compiled=0
n_reuse=0
n_match=0
n_mismatch=0
n_expfail=0
n_corrfail=0
n_internal=0
n_error=0
n_observe=0
fails=()

join_why() {
	local out=$1
	shift
	local w
	for w in "$@"; do out+="; $w"; done
	printf '%s' "$out"
}

eval_row() {
	local name=$1 mlabel=$2 pkg=$3 present=$4 d=$5 s=$6 c=$7 r=$8 spec=$9
	local -a why=()
	local st=ok shown=$d
	n_rows=$((n_rows + 1))
	if [[ $present == 1 ]]; then
		n_compiled=$((n_compiled + 1))
		if [[ $d == reuse ]]; then n_reuse=$((n_reuse + 1)); fi
		if [[ $s == match ]]; then n_match=$((n_match + 1)); fi
		if [[ $s == mismatch* ]]; then
			n_mismatch=$((n_mismatch + 1))
			why+=("shadow $s (blobs in GOCUT_DIR/mismatch)")
		fi
		if [[ $d == *compile-failed* ]]; then
			why+=("compile failed")
		fi
		if [[ $r =~ $internal_re ]]; then
			n_internal=$((n_internal + 1))
			why+=("gocut internal error")
		fi
	else
		shown='(not built)'
	fi
	if ! spec_matches "$spec" "$present" "$d" "$s" "$r"; then
		n_expfail=$((n_expfail + 1))
		why+=("expected $spec, got $shown/$s")
	fi
	if ((${#why[@]} > 0)); then
		st=FAIL
		fails+=("$name [$mlabel] $pkg: $(join_why "${why[@]}") -- reason: $r")
	elif [[ $spec == observe ]]; then
		st=obs
		n_observe=$((n_observe + 1))
	fi
	row "$name" "$mlabel" "$pkg" "$shown" "$s" "$c" "$spec" "$st" "${r:0:$reason_width}"
}

error_row() {
	local name=$1 mlabel=$2 what=$3 log=$4 line
	n_error=$((n_error + 1))
	fails+=("$name [$mlabel]: $what (log: $log)")
	row "$name" "$mlabel" "-" "ERROR" "-" "-" "-" "FAIL" "$what"
	if [[ -f $log ]]; then
		while IFS= read -r line; do
			printf '      | %s\n' "$line"
		done < <(tail -n 25 -- "$log")
	fi
}

run_one() {
	local name=$1 mode=$2
	local fn=${name//-/_}
	local mlabel=default
	local -a flags=()
	case $mode in
	nl)
		mlabel='-N -l'
		flags=('-gcflags=ex/...=-N -l')
		;;
	nld)
		mlabel='-N -l nodwarf'
		flags=('-gcflags=ex/...=-N -l -dwarf=false')
		;;
	esac
	if [[ -n $TOY_GO_P ]]; then
		flags+=(-p "$TOY_GO_P")
	fi
	local dir="$root/$name.$mode"
	local src="$dir/src" gdir="$dir/gocut" cache="$dir/gocache"
	mkdir -p -- "$dir"
	write_tree "$src"

	if ! build_gocut "$src" "$cache" "$gdir" "$dir/initial.bin" "$dir/initial.log" \
		${flags[@]+"${flags[@]}"}; then
		error_row "$name" "$mlabel" "initial build with gocut failed" "$dir/initial.log"
		return 0
	fi
	if ! "edit_$fn" "$src" >"$dir/edit.log" 2>&1; then
		error_row "$name" "$mlabel" "edit failed" "$dir/edit.log"
		return 0
	fi
	if ! GOCUT_DIR="$gdir" "$GOCUT" clear-log >"$dir/clear-log.log" 2>&1; then
		error_row "$name" "$mlabel" "gocut clear-log failed" "$dir/clear-log.log"
		return 0
	fi

	local rebuilt=1 plain_built=1
	if ! build_gocut "$src" "$cache" "$gdir" "$dir/gocut.bin" "$dir/rebuild.log" \
		${flags[@]+"${flags[@]}"}; then
		rebuilt=0
		error_row "$name" "$mlabel" "rebuild with gocut failed" "$dir/rebuild.log"
	fi
	if ! build_plain "$src" "$dir/plain.bin" "$dir/plain.log" ${flags[@]+"${flags[@]}"}; then
		plain_built=0
		error_row "$name" "$mlabel" "plain build failed" "$dir/plain.log"
	fi

	local -A E=([ex/a]=no-record [ex/b]=observe [ex/p]=observe [ex/q]=observe [main]=observe)
	local pkg spec
	while read -r pkg spec; do
		if [[ -z $pkg ]]; then continue; fi
		E[$pkg]=$spec
	done < <("exp_$fn" "$mode")

	local -A D=() S=() C=() R=()
	local ev_pkg ev_d ev_s ev_c ev_r
	while IFS=$'\t' read -r ev_pkg ev_d ev_s ev_c ev_r; do
		D[$ev_pkg]=$ev_d
		S[$ev_pkg]=$ev_s
		C[$ev_pkg]=$ev_c
		R[$ev_pkg]=$ev_r
	done < <(parse_events "$gdir/events.jsonl")

	local -a pkgs=(ex/a ex/b ex/p ex/q main)
	local k
	if ((${#D[@]} > 0)); then
		for k in "${!D[@]}"; do
			case " ${pkgs[*]} " in
			*" $k "*) ;;
			*) pkgs+=("$k") ;;
			esac
		done
	fi

	local present d s c r
	for pkg in "${pkgs[@]}"; do
		spec=${E[$pkg]:-observe}
		present=0 d=- s=- c=- r=-
		if [[ -n ${D[$pkg]+set} ]]; then
			present=1 d=${D[$pkg]} s=${S[$pkg]} c=${C[$pkg]} r=${R[$pkg]}
		fi
		eval_row "$name" "$mlabel" "$pkg" "$present" "$d" "$s" "$c" "$r" "$spec"
	done

	if [[ $rebuilt == 1 && $plain_built == 1 ]]; then
		local got want_out
		got=$("$dir/gocut.bin" 2>&1) || got+=$'\n'"[exit $?]"
		want_out=$("$dir/plain.bin" 2>&1) || want_out+=$'\n'"[exit $?]"
		printf '%s\n' "$got" >"$dir/gocut.out"
		printf '%s\n' "$want_out" >"$dir/plain.out"
		local flat_got=${got//$'\n'/ | } flat_want=${want_out//$'\n'/ | }
		if [[ $got == "$want_out" ]]; then
			row "$name" "$mlabel" "(bin)" "same" "-" "-" "= plain build" "ok" "$flat_got"
		else
			n_corrfail=$((n_corrfail + 1))
			fails+=("$name [$mlabel] binary output differs: gocut=[$flat_got] plain=[$flat_want]")
			row "$name" "$mlabel" "(bin)" "DIFFERS" "-" "-" "= plain build" "FAIL" \
				"gocut=[$flat_got] plain=[$flat_want]"
		fi
	fi
	return 0
}

main() {
	local list_only=0 arg name fn mode
	local -a selected=() modes=()
	for arg in "$@"; do
		case $arg in
		-h | --help)
			usage
			exit 0
			;;
		--list) list_only=1 ;;
		-*) die "unknown option: $arg (see --help)" ;;
		*) selected+=("$arg") ;;
		esac
	done

	for name in "${SCENARIOS[@]}"; do
		fn=${name//-/_}
		declare -F "edit_$fn" >/dev/null || die "internal: missing edit_$fn"
		declare -F "exp_$fn" >/dev/null || die "internal: missing exp_$fn"
		[[ -n ${DESC[$name]+set} ]] || die "internal: missing description for $name"
	done

	if [[ $list_only == 1 ]]; then
		for name in "${SCENARIOS[@]}"; do
			printf '%-25s %s\n' "$name" "${DESC[$name]}"
		done
		exit 0
	fi

	if ((${#selected[@]} == 0)); then
		selected=("${SCENARIOS[@]}")
	else
		for name in "${selected[@]}"; do
			[[ -n ${DESC[$name]+set} ]] || die "unknown scenario: $name (see --list)"
		done
	fi

	read -r -a modes <<<"$MODES"
	((${#modes[@]} > 0)) || die "MODES is empty"
	for mode in "${modes[@]}"; do
		case $mode in
		default | nl | nld) ;;
		*) die "unknown mode in MODES: $mode (use default, nl and/or nld)" ;;
		esac
	done

	command -v go >/dev/null || die "go not found in PATH"
	command -v python3 >/dev/null || die "python3 not found in PATH"
	if [[ $GOCUT != */* ]]; then
		GOCUT=$(command -v -- "$GOCUT") || die "gocut not found in PATH"
	fi
	[[ -x $GOCUT ]] || die "gocut binary not executable: $GOCUT (cargo build --release, or set GOCUT=...)"
	GOCUT=$(realpath -- "$GOCUT")
	[[ $GOCUT != *[[:space:]]* ]] || die "GOCUT path must not contain whitespace: $GOCUT"

	root=$(mktemp -d "${TMPDIR:-/tmp}/gocut-toy.XXXXXX")
	root=$(realpath -- "$root")
	plain_cache="$root/plain-gocache"

	local go_version
	go_version=$(cd -- "$root" && GOTOOLCHAIN=local GOWORK=off go env GOVERSION)
	if [[ $go_version =~ go([0-9]+)\.([0-9]+) ]]; then
		go_line="${BASH_REMATCH[1]}.${BASH_REMATCH[2]}"
	else
		go_line=1.22
	fi

	printf 'gocut:      %s (GOCUT_MODE=%s)\n' "$GOCUT" "$GOCUT_MODE"
	printf 'go:         %s (go.mod: go %s)\n' "$go_version" "$go_line"
	printf 'flag modes: %s   (nl = -gcflags=ex/...=-N -l)\n' "${modes[*]}"
	printf 'work dir:   %s\n\n' "$root"
	row SCENARIO FLAGS PKG DECISION SHADOW CHG EXPECT ST REASON

	for name in "${selected[@]}"; do
		for mode in "${modes[@]}"; do
			run_one "$name" "$mode"
		done
	done

	printf '\n== summary ==\n'
	printf 'rows:                  %d (compiled %d, reuse decisions %d, observe-only %d)\n' \
		"$n_rows" "$n_compiled" "$n_reuse" "$n_observe"
	printf 'shadow matches:        %d\n' "$n_match"
	printf 'shadow mismatches:     %d\n' "$n_mismatch"
	printf 'expectation misses:    %d\n' "$n_expfail"
	printf 'binary output differs: %d\n' "$n_corrfail"
	printf 'gocut internal errors: %d\n' "$n_internal"
	printf 'build/edit errors:     %d\n' "$n_error"

	local f
	if ((${#fails[@]} > 0)); then
		printf '\nEXPECTATION FAILURES (%d):\n' "${#fails[@]}"
		for f in "${fails[@]}"; do
			printf '  - %s\n' "$f"
		done
		printf '\nwork dir kept: %s\n' "$root"
		exit 1
	fi
	printf '\nEXPECTATION FAILURES: none\n'
	if [[ $KEEP == 1 ]]; then
		printf 'work dir kept: %s\n' "$root"
	else
		rm -rf -- "$root"
	fi
}

root=
plain_cache=
go_line=
main "$@"
