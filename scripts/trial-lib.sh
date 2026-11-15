# Shared helpers for scripts/trial-rewrite.sh and scripts/trial-no-space.sh (gate 4.9 fault trials).
# Sourced, never run. Expects WORK (a scratch directory on the runner's own disk) to be set.
# shellcheck shell=bash disable=SC2154 # WORK and RESULTS are set by the script that sources this file.

# A verdict line for one arm, also appended to the results file the main script reads.
verdict() {
	local arm=$1 outcome=$2 detail=$3
	printf 'RESULT arm=%s outcome=%s detail=%s\n' "$arm" "$outcome" "$detail"
	printf '%s\t%s\t%s\n' "$arm" "$outcome" "$detail" >>"$RESULTS"
}

die() {
	printf 'ERROR: %s\n' "$*" >&2
	exit 1
}

need_tools() {
	local t
	for t in "$@"; do
		command -v "$t" >/dev/null 2>&1 || die "required tool '$t' is not installed"
	done
}

need_module() {
	local m=$1
	modprobe "$m" 2>/dev/null || grep -qw "$m" /proc/filesystems 2>/dev/null ||
		die "kernel module '$m' could not be loaded (modprobe $m failed) and is not built in"
}

# bg_start NAME CMD...: runs CMD in the background with its standard input on a FIFO and its output in
# $WORK/NAME.out. Sets BG_PID and BG_FD (the FIFO's write end).
bg_start() {
	local name=$1
	shift
	rm -f "$WORK/$name.in" "$WORK/$name.out"
	mkfifo "$WORK/$name.in"
	: >"$WORK/$name.out"
	"$@" <"$WORK/$name.in" >"$WORK/$name.out" 2>&1 &
	BG_PID=$!
	exec {BG_FD}>"$WORK/$name.in"
}

# bg_send: one line to the background command.
bg_send() {
	printf 'go\n' >&"$BG_FD"
}

# bg_wait_for NAME REGEX SECONDS: waits until NAME's output has a line matching REGEX; fails if the command exits or
# the time runs out first.
bg_wait_for() {
	local name=$1 re=$2 secs=$3 i
	for ((i = 0; i < secs * 10; i++)); do
		if grep -Eq "$re" "$WORK/$name.out"; then
			return 0
		fi
		if ! kill -0 "$BG_PID" 2>/dev/null; then
			grep -Eq "$re" "$WORK/$name.out" && return 0
			show_out "$name"
			die "$name exited before printing /$re/"
		fi
		sleep 0.1
	done
	show_out "$name"
	die "$name did not print /$re/ within ${secs}s"
}

# bg_finish NAME SECONDS: closes the input and waits for the command; prints its output; sets BG_STATUS.
bg_finish() {
	local name=$1 secs=$2 i
	exec {BG_FD}>&-
	for ((i = 0; i < secs * 10; i++)); do
		kill -0 "$BG_PID" 2>/dev/null || break
		sleep 0.1
	done
	if kill -0 "$BG_PID" 2>/dev/null; then
		kill "$BG_PID" 2>/dev/null || true
		printf '%s did not exit within %ss: killed\n' "$name" "$secs"
	fi
	BG_STATUS=0
	wait "$BG_PID" || BG_STATUS=$?
	show_out "$name"
	printf '%s exit status %s\n' "$name" "$BG_STATUS"
}

show_out() {
	sed "s/^/  | /" "$WORK/$1.out"
}

# field LINE KEY: the value of KEY=value in LINE.
field() {
	local line=$1 key=$2
	sed -n "s/.*[[:space:]]$key=\([^[:space:]]*\).*/\1/p" <<<" $line" | head -n 1
}

# last_line NAME REGEX: the last line of NAME's output matching REGEX (empty if none).
last_line() {
	grep -E "$2" "$WORK/$1.out" | tail -n 1 || true
}

# run NAME CMD...: runs CMD in the foreground, output to $WORK/NAME.out, shown indented; sets RUN_STATUS.
run() {
	local name=$1
	shift
	RUN_STATUS=0
	"$@" >"$WORK/$name.out" 2>&1 || RUN_STATUS=$?
	show_out "$name"
	printf '%s exit status %s\n' "$name" "$RUN_STATUS"
}
