#!/usr/bin/env bash
# Q-52 (b) trial (gate 4.9): does writing identical bytes again make the next sync write them?
#
#   sudo bash scripts/trial-rewrite.sh <ext4|xfs> <path to fault_trial> [transcript]
#
# Linux only, as root. A 1 GiB loop image under a device-mapper `linear` device holds the file system. To fail one
# file, the table becomes linear | error | linear over exactly that file's extents (`filefrag`), so the journal and
# every other file stay writable. Arms, each on a fresh file system:
#   probe     the bare platform fact: a write over failed sectors, a failed sync, then a sync alone (control) and the
#             same bytes written again and synced (treatment); the device is read with O_DIRECT after each.
#   batch     the engine: a batch whose segment sync fails; the mark removed; a start on the same boot writes the
#             last batch again; the device must then hold the file's bytes.
#   mark      the same with the fault on the mark: the start writes the mark slot it read again.
#   manifest  the same with the fault on the manifest while a start records a lost range: the next start writes
#             the last manifest record again.
# Every arm runs; each prints one RESULT line (PASS, FAIL or INCONCLUSIVE); the exit status is non-zero unless all
# pass. A missing tool or kernel module fails the run; nothing is skipped.
# shellcheck disable=SC2154 # WORK, FS, BIN and RESULTS reach the child process through its environment.
set -euo pipefail

usage() {
	printf 'usage: %s <ext4|xfs> <fault_trial binary> [transcript]\n' "$0" >&2
	exit 2
}

ARMS=(probe batch mark manifest)
BOOT1=11111111111111111111111111111111
BOOT2=22222222222222222222222222222222
BOOT3=33333333333333333333333333333333
SEG_SIZE=16777216

# ----- one arm, in a child process with its own device and cleanup -----

arm_setup() {
	IMG=$WORK/img
	MNT=$WORK/mnt
	DM=kvt-$ARM-$$
	DEV=/dev/mapper/$DM
	LOOP=
	mkdir -p "$MNT"
	trap arm_cleanup EXIT
	truncate -s 1G "$IMG"
	LOOP=$(losetup --show -f "$IMG")
	SECTORS=$(blockdev --getsz "$LOOP")
	dmsetup create "$DM" --table "0 $SECTORS linear $LOOP 0"
	case $FS in
	ext4) mkfs.ext4 -q -F "$DEV" ;;
	xfs) mkfs.xfs -q -f "$DEV" ;;
	esac
	mount "$DEV" "$MNT"
	printf 'arm %s: %s on %s (loop %s, %s sectors)\n' "$ARM" "$FS" "$DEV" "$LOOP" "$SECTORS"
}

# shellcheck disable=SC2317 # run by the EXIT trap
arm_cleanup() {
	local st=$?
	set +e
	[[ -n ${BG_PID:-} ]] && kill "$BG_PID" 2>/dev/null
	mountpoint -q "$MNT" && umount "$MNT"
	dmsetup info "$DM" >/dev/null 2>&1 && dmsetup remove "$DM"
	[[ -n ${LOOP:-} ]] && losetup -d "$LOOP"
	rm -f "$IMG"
	exit "$st"
}

remount() {
	umount "$MNT"
	mount "$DEV" "$MNT"
	printf 'remounted %s\n' "$MNT"
}

# extents FILE: writes "logical physical length" lines (512-byte sectors, clipped at the end of the file) to
# $WORK/extents and checks that they cover the file exactly and are ordinary written extents. It never syncs the
# file: a sync from outside could write bytes the engine left dirty and hide a missing sync.
extents() {
	local file=$1 size bs rounded total
	size=$(stat -c %s "$file")
	bs=$(stat -f -c %S "$file")
	rounded=$(((size + bs - 1) / bs * bs))
	filefrag -e -b512 "$file" >"$WORK/filefrag"
	awk -v limit=$((rounded / 512)) '
		/^ *[0-9]+:/ {
			line = $0
			gsub(/\.\./, " ", line); gsub(/:/, " ", line)
			n = split(line, f, " ")
			if (f[2] >= limit) next
			if (line ~ /unknown|delalloc|inline|unwritten|encoded|not_aligned/) { bad = 1 }
			len = f[6]; if (f[2] + len > limit) len = limit - f[2]
			print f[2], f[4], len
		}
		END { if (bad) exit 3 }' "$WORK/filefrag" >"$WORK/extents" || {
		cat "$WORK/filefrag"
		die "$file has extents that are not ordinary written extents"
	}
	total=$(awk '{ s += $3 } END { print s + 0 }' "$WORK/extents")
	if ((total * 512 != rounded)); then
		cat "$WORK/filefrag"
		die "extents of $file cover $((total * 512)) bytes, expected $rounded (size $size, block $bs)"
	fi
}

load_table() {
	dmsetup suspend "$@" "$DM"
	dmsetup load "$DM" "$WORK/table"
	dmsetup resume "$DM"
}

# fail_file FILE: the device fails reads and writes of exactly FILE's sectors. Suspending flushes the file system
# first, so nothing written before is lost to the error target.
fail_file() {
	local file=$1 cur=0 start len
	extents "$file"
	: >"$WORK/table"
	while read -r start len; do
		if ((start > cur)); then
			printf '%s %s linear %s %s\n' "$cur" "$((start - cur))" "$LOOP" "$cur" >>"$WORK/table"
		fi
		printf '%s %s error\n' "$start" "$len" >>"$WORK/table"
		cur=$((start + len))
	done < <(awk '{ print $2, $3 }' "$WORK/extents" | sort -n)
	if ((cur < SECTORS)); then
		printf '%s %s linear %s %s\n' "$cur" "$((SECTORS - cur))" "$LOOP" "$cur" >>"$WORK/table"
	fi
	load_table
	printf 'failing %s:\n' "$file"
	sed 's/^/  table: /' "$WORK/table"
}

# restore_device: the plain linear table again. No file-system freeze: a freeze would write back through the error
# target.
restore_device() {
	printf '0 %s linear %s 0\n' "$SECTORS" "$LOOP" >"$WORK/table"
	load_table --nolockfs
	printf 'device restored to linear\n'
}

# dev_read FILE OFFSET LENGTH OUT: FILE's bytes at [OFFSET, OFFSET + LENGTH) read from the device with O_DIRECT.
dev_read() {
	local file=$1 off=$2 len=$3 out=$4 s0 s1 ls ps n a b got
	extents "$file"
	s0=$((off / 512))
	s1=$(((off + len + 511) / 512))
	: >"$out.raw"
	while read -r ls ps n; do
		a=$((ls > s0 ? ls : s0))
		b=$((ls + n < s1 ? ls + n : s1))
		if ((a < b)); then
			dd if="$DEV" of="$out.raw" iflag=direct oflag=append conv=notrunc bs=512 \
				skip=$((ps + a - ls)) count=$((b - a)) status=none
		fi
	done <"$WORK/extents"
	got=$(stat -c %s "$out.raw")
	((got == (s1 - s0) * 512)) || die "direct read of $file returned $got bytes, expected $(((s1 - s0) * 512))"
	tail -c +$((off - s0 * 512 + 1)) "$out.raw" | head -c "$len" >"$out"
}

# cached_read FILE OFFSET LENGTH OUT: the same bytes through the page cache.
cached_read() {
	dd if="$1" of="$4" iflag=skip_bytes,count_bytes skip="$2" count="$3" status=none
}

# same_on_device FILE: sets SAME to 1 if every byte of FILE in the page cache is on the device, else 0. (Never
# called as a condition, so that errexit holds inside.)
same_on_device() {
	local file=$1 size
	size=$(stat -c %s "$file")
	dev_read "$file" 0 "$size" "$WORK/dev.bin"
	cached_read "$file" 0 "$size" "$WORK/cache.bin"
	if cmp -s "$WORK/dev.bin" "$WORK/cache.bin"; then
		printf '%s: device and page cache agree (%s bytes)\n' "$file" "$size"
		SAME=1
		return
	fi
	SAME=0
	printf '%s: device and page cache differ:\n' "$file"
	cmp -l "$WORK/dev.bin" "$WORK/cache.bin" | head -n 5 | sed 's/^/  offset+1 device cache (octal): /' || true
}

arm_probe() {
	local f=$MNT/probe.bin line off len
	bg_start probe "$BIN" probe "$f"
	bg_wait_for probe 'step=ready' 60
	line=$(last_line probe 'step=ready')
	off=$(field "$line" offset)
	len=$(field "$line" len)
	head -c "$len" /dev/zero | tr '\0' '\245' >"$WORK/pattern.bin"
	fail_file "$f"
	bg_send
	bg_wait_for probe 'step=fault-sync' 60
	restore_device
	if ! grep -q 'step=fault-write result=ok' "$WORK/probe.out"; then
		bg_finish probe 10
		verdict probe INCONCLUSIVE "the write over the failed sectors failed: the page was not cached"
		return
	fi
	if ! grep -q 'step=fault-sync result=err' "$WORK/probe.out"; then
		bg_finish probe 10
		verdict probe INCONCLUSIVE "the sync over the failed sectors did not fail: the error target did not apply"
		return
	fi
	bg_send
	bg_wait_for probe 'step=control-sync' 60
	if ! grep -q 'step=control-sync result=ok' "$WORK/probe.out"; then
		bg_finish probe 10
		verdict probe FAIL "the control sync after restoring the device failed"
		return
	fi
	local control
	dev_read "$f" "$off" "$len" "$WORK/control.bin"
	if cmp -s "$WORK/control.bin" "$WORK/pattern.bin"; then
		control="the device holds the new bytes after the control sync: the kernel kept the page dirty"
	else
		control="the device does not hold the new bytes after the control sync: the failed sync left the page clean"
	fi
	printf 'control: %s\n' "$control"
	bg_send
	bg_wait_for probe 'step=treatment-sync' 60
	bg_send
	bg_finish probe 30
	if ! grep -q 'step=treatment-sync result=ok' "$WORK/probe.out"; then
		verdict probe FAIL "the treatment sync failed; control: $control"
		return
	fi
	dev_read "$f" "$off" "$len" "$WORK/treatment.bin"
	if cmp -s "$WORK/treatment.bin" "$WORK/pattern.bin"; then
		verdict probe PASS "writing the bytes again made the sync write them; control: $control"
	else
		verdict probe FAIL "the device does not hold the bytes written again and synced; control: $control"
	fi
}

# engine_fault WHAT EXPECT_OP: writes 20 acknowledged calls, fails the file named by the paused line's WHAT field,
# lets the writer go on, and checks that it stops at a fault of EXPECT_OP with nothing more acknowledged. Sets
# FAULT_LINE, and ENGINE_OK to 0 after a verdict if the arm cannot go on.
engine_fault() {
	local what=$1 expect=$2 line file acked op
	ENGINE_OK=0
	STORE=$MNT/store
	ACKS=$WORK/acks
	rm -f "$ACKS"
	bg_start w "$BIN" write "$STORE" "$ACKS" --pause-after 20 --batches 200 --segment-size "$SEG_SIZE" \
		--boot-id "$BOOT1" --timeout 60
	bg_wait_for w 'write paused' 120
	line=$(last_line w 'write paused')
	file=$(field "$line" "$what")
	[[ -n $file && -f $file ]] || die "the paused line names no $what file: $line"
	fail_file "$file"
	bg_send
	bg_finish w 120
	restore_device
	acked=$(wc -l <"$ACKS")
	FAULT_LINE=$(last_line w 'write fault ')
	if ((BG_STATUS != 3)) || [[ -z $FAULT_LINE ]]; then
		verdict "$ARM" INCONCLUSIVE "the writer did not stop at a fault (exit status $BG_STATUS)"
		return
	fi
	op=$(field "$FAULT_LINE" op)
	if [[ $op != "$expect" ]]; then
		verdict "$ARM" INCONCLUSIVE "the fault was at $op, not $expect: the trial did not fail the sync it aims at"
		return
	fi
	if ((acked != 20)); then
		verdict "$ARM" FAIL "$acked calls acknowledged, expected 20: a call was acknowledged after the fault"
		return
	fi
	printf 'fault as expected: %s\n' "$FAULT_LINE"
	ENGINE_OK=1
}

# after_remount [VERIFY OPTIONS]: a remount (the page cache gone), then verify, a start under a new boot id, and
# verify again. Sets REMOUNT_OK to 1 if all three succeed.
after_remount() {
	REMOUNT_OK=0
	remount
	run verify1 "$BIN" verify "$STORE" "$ACKS" --boot-id "$BOOT2" "$@"
	((RUN_STATUS == 0)) || return 0
	run start2 "$BIN" start "$STORE" --boot-id "$BOOT3"
	((RUN_STATUS == 0)) || return 0
	run verify2 "$BIN" verify "$STORE" "$ACKS" --boot-id "$BOOT3" "$@"
	((RUN_STATUS == 0)) || return 0
	REMOUNT_OK=1
}

arm_batch() {
	local seg
	engine_fault file Sync
	((ENGINE_OK == 1)) || return 0
	seg=$(field "$(last_line w '^last_batch ')" file)
	[[ -n $seg ]] || seg=$(field "$(last_line w 'write paused')" file)
	printf 'faulted batch (read through the page cache): %s\n' "$(last_line w '^last_batch ')"
	rm "$STORE/log/MARK"
	run start1 "$BIN" start "$STORE" --boot-id "$BOOT1"
	if ((RUN_STATUS != 0)); then
		verdict batch FAIL "the same-boot start after the fault failed"
		return
	fi
	grep -q 'rewrote_last_batch=true' "$WORK/start1.out" ||
		printf 'note: the start did not report writing the last batch again\n'
	same_on_device "$seg"
	if ((SAME == 0)); then
		verdict batch FAIL "after the start's rewrite the device does not hold the segment's bytes"
		return
	fi
	after_remount
	if ((REMOUNT_OK == 0)); then
		verdict batch FAIL "an acknowledged position is missing after the remount, or a start failed"
		return
	fi
	verdict batch PASS "the start wrote the last batch again and the device holds it; every acknowledged position read back"
}

arm_mark() {
	engine_fault mark MarkSync
	((ENGINE_OK == 1)) || return 0
	run start1 "$BIN" start "$STORE" --boot-id "$BOOT1"
	if ((RUN_STATUS != 0)); then
		verdict mark FAIL "the same-boot start after the fault failed"
		return
	fi
	same_on_device "$STORE/log/MARK"
	if ((SAME == 0)); then
		verdict mark FAIL "after the start's rewrite the device does not hold the mark's bytes"
		return
	fi
	after_remount
	if ((REMOUNT_OK == 0)); then
		verdict mark FAIL "an acknowledged position is missing after the remount, or a start failed"
		return
	fi
	verdict mark PASS "the start wrote the mark slot again and the device holds it; every acknowledged position read back"
}

arm_manifest() {
	STORE=$MNT/store
	ACKS=$WORK/acks
	rm -f "$ACKS"
	run w "$BIN" write "$STORE" "$ACKS" --batches 30 --segment-size "$SEG_SIZE" --boot-id "$BOOT1"
	((RUN_STATUS == 0)) || die "writing the store failed"
	run damage "$BIN" damage "$STORE"
	((RUN_STATUS == 0)) || die "damaging a batch failed"
	fail_file "$STORE/MANIFEST"
	run start1 "$BIN" start "$STORE" --boot-id "$BOOT1"
	restore_device
	local fault
	fault=$(last_line start1 'start fault ')
	if ((RUN_STATUS != 3)) || [[ $(field "$fault" path) != */MANIFEST ]]; then
		verdict manifest INCONCLUSIVE "the start recording the lost range did not fail on the manifest (exit status $RUN_STATUS)"
		return
	fi
	if ! grep -q 'start loss .*cause=Corruption' "$WORK/start1.out"; then
		verdict manifest INCONCLUSIVE "the start found no lost range to record"
		return
	fi
	run start2 "$BIN" start "$STORE" --boot-id "$BOOT1"
	if ((RUN_STATUS != 0)); then
		verdict manifest FAIL "the same-boot start after the fault failed"
		return
	fi
	same_on_device "$STORE/MANIFEST"
	if ((SAME == 0)); then
		verdict manifest FAIL "after the start's rewrite the device does not hold the manifest's bytes"
		return
	fi
	after_remount --allow-recorded-losses
	if ((REMOUNT_OK == 0)); then
		verdict manifest FAIL "an acknowledged position outside the recorded loss is missing, or a start failed"
		return
	fi
	if ! grep -q 'lost_ranges=\[(' "$WORK/verify2.out"; then
		verdict manifest FAIL "the lost range is not in the manifest after the remount"
		return
	fi
	verdict manifest PASS "the start wrote the manifest record again and the device holds it; the lost range survived the remount"
}

child() {
	ARM=$1
	set -o errtrace
	trap 'printf "error at line %s: %s\n" "$LINENO" "$BASH_COMMAND"' ERR
	arm_setup
	case $ARM in
	probe) arm_probe ;;
	batch) arm_batch ;;
	mark) arm_mark ;;
	manifest) arm_manifest ;;
	*) die "unknown arm $ARM" ;;
	esac
}

# ----- main -----

main() {
	[[ $# -ge 2 && $# -le 3 ]] || usage
	FS=$1
	BIN=$(realpath "$2")
	case $FS in ext4 | xfs) ;; *) usage ;; esac
	[[ $(uname -s) == Linux ]] || die "Linux only"
	((EUID == 0)) || die "run as root"
	[[ -x $BIN ]] || die "$BIN is not an executable"
	need_tools losetup dmsetup blockdev filefrag dd cmp mkfifo mountpoint "mkfs.$FS" modprobe stat awk
	need_module dm_mod
	need_module "$FS"
	dmsetup targets | grep -q '^error' || die "device-mapper has no error target"
	dmsetup targets | grep -q '^linear' || die "device-mapper has no linear target"

	printf '# Q-52 (b) rewrite trial on %s\n' "$FS"
	printf 'kernel: %s\n' "$(uname -r)"
	printf 'device-mapper: %s\n' "$(dmsetup version | tr '\n' ' ')"
	case $FS in
	ext4) printf 'e2fsprogs: %s\n' "$(mkfs.ext4 -V 2>&1 | head -n 1)" ;;
	xfs) printf 'xfsprogs: %s\n' "$(mkfs.xfs -V 2>&1 | head -n 1)" ;;
	esac
	printf 'date: %s\n\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"

	local base arm st
	base=$(mktemp -d /var/tmp/kvik-trial.XXXXXX)
	export RESULTS=$base/results
	: >"$RESULTS"
	for arm in "${ARMS[@]}"; do
		printf '\n## arm %s\n' "$arm"
		mkdir -p "$base/$arm"
		st=0
		WORK=$base/$arm FS=$FS BIN=$BIN bash "$0" --child "$arm" || st=$?
		if ! grep -q "^$arm"$'\t' "$RESULTS"; then
			verdict "$arm" FAIL "the arm stopped before a verdict (exit status $st)"
		fi
	done

	printf '\n## summary (%s, kernel %s)\n' "$FS" "$(uname -r)"
	local failed=0 outcome detail
	while IFS=$'\t' read -r arm outcome detail; do
		printf '%-9s %-12s %s\n' "$arm" "$outcome" "$detail"
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

TRANSCRIPT=${3:-${TRIAL_TRANSCRIPT:-}}
if [[ -n $TRANSCRIPT ]]; then
	exec > >(tee -a "$TRANSCRIPT") 2>&1
fi
main "$@"
