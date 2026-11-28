#!/usr/bin/env sh
# Runs the ARM version of tom-os on an Android phone, inside QEMU in Termux
# (https://termux.dev - install it from F-Droid). No root is needed. QEMU
# emulates the machine, so this is slow, but it is the real ARM kernel.
#
#   sh android-termux.sh          text mode, in this terminal
#   sh android-termux.sh gui      graphical mode, upright like the phone
#                                 (TOMOS_RES=1024x768 TOMOS_SCALE=1 for a
#                                 landscape screen): connect a VNC viewer app
#                                 to 127.0.0.1:5900, password "tomos"
#                                 (TOMOS_VNC_PASSWORD changes it; VNC uses
#                                 at most 8 characters)
#
# The LLM: by default a small stand-in that needs no key is started on the
# phone (it echoes what you say, and writes a little app when the request
# contains the word "app"). To use a real one, run a gateway somewhere the
# phone can reach, e.g. tools/claude_gateway.py --bind 0.0.0.0 on a PC in
# the same network, and pass its address:
#
#   TOMOS_LLM=192.168.1.20:8080 sh android-termux.sh gui
#
# Quit QEMU in text mode with Ctrl-A then X, or type /halt in tom-os.
set -e

URL=${TOMOS_URL:-https://tomsonoda.github.io/tom-os/downloads}
DIR=${TOMOS_DIR:-$HOME/tom-os}
MODE=${1:-text}
MEM=${TOMOS_MEM:-256M}

need() { # command, Termux package
    command -v "$1" >/dev/null 2>&1 && return
    if command -v pkg >/dev/null 2>&1; then
        pkg install -y "$2"
    else
        echo "Please install $1." >&2
        exit 1
    fi
}
need qemu-system-aarch64 qemu-system-aarch64-headless
need curl curl

mkdir -p "$DIR"
cd "$DIR"
echo "Downloading tom-os (ARM) ..."
curl -fL --progress-bar -o Image.new "$URL/tomos-aarch64-Image" && mv Image.new Image

if [ -n "$TOMOS_LLM" ]; then
    LLM=$TOMOS_LLM
    MODEL=${TOMOS_MODEL:-default}
else
    need python3 python
    curl -fsL -o mock_llm.py "$URL/mock_llm.py"
    python3 mock_llm.py 8080 >/dev/null 2>&1 &
    STAND_IN=$!
    trap 'kill $STAND_IN 2>/dev/null' EXIT
    LLM=10.0.2.2:8080 # the phone itself, as seen from inside QEMU
    MODEL=mock
    echo "No TOMOS_LLM given: using the stand-in model on this phone."
fi

MACHINE="-M virt -cpu cortex-a72 -m $MEM -no-reboot -kernel Image
  -global virtio-mmio.force-legacy=false
  -netdev user,id=n0 -device virtio-net-device,netdev=n0"

if [ "$MODE" = gui ]; then
    PASSWORD=${TOMOS_VNC_PASSWORD:-tomos}
    echo "Graphical mode: connect a VNC viewer to 127.0.0.1:5900, password: $PASSWORD"
    echo "This terminal is tom-os's serial console as well: what you type here,"
    echo "with the phone's own keyboard (Japanese too), goes to the tom-os prompt."
    # shellcheck disable=SC2086
    qemu-system-aarch64 $MACHINE \
        -device ramfb -device virtio-keyboard-device -device virtio-tablet-device \
        -object "secret,id=vnc,data=$PASSWORD" -display none -vnc 127.0.0.1:0,password-secret=vnc -serial mon:stdio \
        -append "llm=$LLM model=$MODEL res=${TOMOS_RES:-720x1600} scale=${TOMOS_SCALE:-2}"
else
    # shellcheck disable=SC2086
    qemu-system-aarch64 $MACHINE -nographic -append "gui=0 llm=$LLM model=$MODEL"
fi
