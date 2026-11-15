#!/usr/bin/env bash
# Q-86 (b) trial (gate 4.9): no space at a log write on a copy-on-write file system.
#
#   sudo bash scripts/trial-no-space.sh <path to fault_trial> [transcript]
#
# Linux only, as root. Btrfs on a 512 MiB loop image. The writer commits with the mark synced at every commit and
# the critical disk mark off (so that it writes into the full file system instead of waiting before it), pauses,
# the file system is filled to capacity, and the writer goes on: the output names which call failed and the error
# kind. Then (a) with the disk still full, a remount, a start (read-only allowed) and a check that every
# acknowledged position reads back; (b) the fill removed, a start, the check, and one new commit. Arms, each on a
# fresh file system:
#   cow    the log directory as created;
#   nocow  the log directory created with copy-on-write off (`chattr +C`) before the first segment.
# Every arm runs; each prints one RESULT line (PASS, FAIL or INCONCLUSIVE); the exit status is non-zero unless all
# pass. A missing tool or kernel module fails the run; nothing is skipped.
# shellcheck disable=SC2154 # WORK, BIN and RESULTS reach the child process through its environment.
set -euo pipefail

usage() {
	printf 'usage: %s <fault_trial binary> [transcript]\n' "$0" >&2
	exit 2
}

ARMS=(cow nocow)
BOOT1=11111111111111111111111111111111
BOOT2=22222222222222222222222222222222
SEG_SIZE=16777216
PER_CALL=1000

arm_setup() {
	IMG=$WORK/img
	MNT=$WORK/mnt
	LOOP=
	mkdir -p "$MNT"
	trap arm_cleanup EXIT
	truncate -s 512M "$IMG"
	LOOP=$(losetup --show -f "$IMG")
	mkfs.btrfs -q -f "$LOOP"
	mount "$LOOP" "$MNT"
	printf 'arm %s: btrfs on %s\n' "$ARM" "$LOOP"
}

# shellcheck disable=SC2317 # run by the EXIT trap
arm_cleanup() {
	local st=$?
	set +e
	[[ -n ${BG_PID:-} ]] && kill "$BG_PID" 2>/dev/null
	mountpoint -q "$MNT" && umount "$MNT"
	[[ -n ${LOOP:-} ]] && losetup -d "$LOOP"
	rm -f "$IMG"
	exit "$st"
}

space() {
	df -B1 --output=size,used,avail "$MNT" | sed 's/^/  df: /'
}

arm_run() {
	local store=$MNT/store acks=$WORK/acks line acked lines observed op kind
	rm -f "$acks"
	if [[ $ARM == nocow ]]; then
		mkdir -p "$store/log"
		chattr +C "$store/log"
		lsattr -d "$store/log"
		lsattr -d "$store/log" | awk '{ print $1 }' | grep -q C || die "chattr +C did not take on $store/log"
	fi
	bg_start w "$BIN" write "$store" "$acks" --pause-after 50 --batches 1000000 --per-call "$PER_CALL" \
		--segment-size "$SEG_SIZE" --disk-critical 0 --timeout 60 --boot-id "$BOOT1"
	bg_wait_for w 'write paused' 120
	space
	printf 'filling the file system\n'
	dd if=/dev/urandom of="$MNT/fill" bs=1M status=none 2>&1 | sed 's/^/  dd: /' || true
	sync -f "$MNT" || true
	space
	bg_send
	bg_finish w 1200

	case $BG_STATUS in
	0 | 3 | 4) ;;
	*)
		verdict "$ARM" FAIL "the writer ended with exit status $BG_STATUS"
		return
		;;
	esac
	line=$(last_line w 'write (stopped|stalled|done) ')
	acked=$(field "$line" acked)
	lines=$(wc -l <"$acks")
	if [[ -z $acked ]] || ((lines != acked * PER_CALL)); then
		verdict "$ARM" FAIL "the acknowledgements recorded ($lines positions) do not match the writer's count (${acked:-none} calls)"
		return
	fi
	case $BG_STATUS in
	3)
		line=$(last_line w 'write fault ')
		op=$(field "$line" op)
		kind=$(field "$line" kind)
		observed="the writer stopped at a fault: op=$op kind=$kind file=$(basename "$(field "$line" path)") after $acked calls"
		if [[ $kind != StorageFull ]]; then
			verdict "$ARM" FAIL "$observed; the error kind is not lack of space"
			return
		fi
		;;
	4)
		observed="no write or sync failed: ingest waited ($(field "$(last_line w 'write stalled')" reasons))"
		if [[ $ARM == cow ]]; then
			verdict "$ARM" INCONCLUSIVE "$observed; the trial did not meet the failure it looks for"
			return
		fi
		;;
	0)
		observed="every call was acknowledged: the file system never ran out"
		verdict "$ARM" INCONCLUSIVE "$observed"
		return
		;;
	esac
	printf 'observed: %s\n' "$observed"

	printf '\n(a) disk still full: remount, start, verify\n'
	umount "$MNT"
	mount "$LOOP" "$MNT"
	space
	run start_a "$BIN" start "$store" --boot-id "$BOOT2" --disk-critical 0
	if ((RUN_STATUS != 0 && RUN_STATUS != 3)); then
		verdict "$ARM" FAIL "$observed; with the disk full the start did not come up (exit status $RUN_STATUS)"
		return
	fi
	run verify_a "$BIN" verify "$store" "$acks" --boot-id "$BOOT2"
	if ((RUN_STATUS != 0)); then
		verdict "$ARM" FAIL "$observed; with the disk full an acknowledged position is missing"
		return
	fi

	printf '\n(b) fill removed: start, verify, one new commit\n'
	rm "$MNT/fill"
	sync -f "$MNT"
	space
	run start_b "$BIN" start "$store" --boot-id "$BOOT2" --disk-critical 0
	if ((RUN_STATUS != 0)); then
		verdict "$ARM" FAIL "$observed; after freeing space the start failed (exit status $RUN_STATUS)"
		return
	fi
	run verify_b "$BIN" verify "$store" "$acks" --boot-id "$BOOT2"
	if ((RUN_STATUS != 0)); then
		verdict "$ARM" FAIL "$observed; after freeing space an acknowledged position is missing"
		return
	fi
	run write_b "$BIN" write "$store" "$acks" --batches 1 --per-call 1 --segment-size "$SEG_SIZE" \
		--disk-critical 0 --boot-id "$BOOT2"
	if ((RUN_STATUS != 0)); then
		verdict "$ARM" FAIL "$observed; after freeing space a new commit failed"
		return
	fi
	run verify_c "$BIN" verify "$store" "$acks" --boot-id "$BOOT2"
	if ((RUN_STATUS != 0)); then
		verdict "$ARM" FAIL "$observed; the new commit does not read back"
		return
	fi
	verdict "$ARM" PASS "$observed; nothing acknowledged was lost, full or freed, and a new commit succeeded"
}

child() {
	ARM=$1
	set -o errtrace
	trap 'printf "error at line %s: %s\n" "$LINENO" "$BASH_COMMAND"' ERR
	arm_setup
	arm_run
}

main() {
	[[ $# -ge 1 && $# -le 2 ]] || usage
	BIN=$(realpath "$1")
	[[ $(uname -s) == Linux ]] || die "Linux only"
	((EUID == 0)) || die "run as root"
	[[ -x $BIN ]] || die "$BIN is not an executable"
	need_tools losetup mkfs.btrfs chattr lsattr df dd mkfifo mountpoint modprobe awk
	need_module btrfs

	printf '# Q-86 (b) no-space trial on btrfs\n'
	printf 'kernel: %s\n' "$(uname -r)"
	printf 'btrfs-progs: %s\n' "$(mkfs.btrfs --version 2>&1 | head -n 1)"
	printf 'date: %s\n\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"

	local base arm st
	base=$(mktemp -d /var/tmp/kvik-trial.XXXXXX)
	export RESULTS=$base/results
	: >"$RESULTS"
	for arm in "${ARMS[@]}"; do
		printf '\n## arm %s\n' "$arm"
		mkdir -p "$base/$arm"
		st=0
		WORK=$base/$arm BIN=$BIN bash "$0" --child "$arm" || st=$?
		if ! grep -q "^$arm"$'\t' "$RESULTS"; then
			verdict "$arm" FAIL "the arm stopped before a verdict (exit status $st)"
		fi
	done

	printf '\n## summary (btrfs, kernel %s)\n' "$(uname -r)"
	local failed=0 outcome detail
	while IFS=$'\t' read -r arm outcome detail; do
		printf '%-6s %-12s %s\n' "$arm" "$outcome" "$detail"
		[[ $outcome == PASS ]] || failed=1
	done <"$RESULTS"
	rm -rf "$base"
	return "$failed"
}

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=scripts/trial-lib.sh
source "$SCRIPT_DIR/trial-lib.sh"

if [[ ${1:-} == --child ]]; then
	child "$2"
	exit 0
fi

TRANSCRIPT=${2:-${TRIAL_TRANSCRIPT:-}}
if [[ -n $TRANSCRIPT ]]; then
	exec > >(tee -a "$TRANSCRIPT") 2>&1
fi
main "$@"
