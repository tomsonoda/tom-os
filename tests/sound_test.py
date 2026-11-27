#!/usr/bin/env python3
"""Sound test: plays tones, a melody, a chord and a stopped tone through the
AC'97 driver, records what QEMU outputs with its WAV backend, and checks the
pitches and timing."""
import math
import os
import struct
import sys
import tempfile
import time
import wave

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(ROOT, "tools"))
sys.path.insert(0, os.path.join(ROOT, "tests"))
import mock_llm  # noqa: E402
import boot_test  # noqa: E402

PROMPT = boot_test.PROMPT

SCRIPT = (
    "sound.tone(440, 600, 'sine') sound.wait() tomos.sleep(400) "
    "sound.melody('C5:300 E5:300 G5:300', 300, 'sine') sound.wait() tomos.sleep(400) "
    "sound.tone(440, 600, 'sine', 0.4) sound.tone(660, 600, 'sine', 0.4) sound.wait() tomos.sleep(400) "
    "sound.tone(330, 5000, 'square') tomos.sleep(300) sound.stop() tomos.sleep(400) "
    "local s = {} for i = 1, 11025 do s[i] = math.sin(i * 2 * math.pi * 1000 / 22050) end "
    "sound.play(s, 22050) sound.wait() print('played')"
)


def goertzel(x, rate, f):
    w = 2 * math.pi * f / rate
    c = 2 * math.cos(w)
    s1 = s2 = 0.0
    for v in x:
        s1, s2 = v + c * s1 - s2, s1
    return math.sqrt(max(s1 * s1 + s2 * s2 - c * s1 * s2, 0)) / len(x)


def segments(samples, rate, quiet=300, gap=0.15):
    """(start, end) sample ranges with sound, split at silences >= gap s."""
    out, start, last = [], None, None
    for i in range(0, len(samples), 64):
        loud = max(abs(v) for v in samples[i:i + 64]) > quiet
        if loud:
            if start is None:
                start = i
            last = i + 64
        elif start is not None and i - last > gap * rate:
            out.append((start, last))
            start = None
    if start is not None:
        out.append((start, last))
    return out


def pitch(x, rate):
    zc = sum(1 for a, b in zip(x, x[1:]) if (a < 0) != (b < 0))
    return zc / 2 / (len(x) / rate)


def main():
    _, port = mock_llm.start()
    wav_path = os.path.join(tempfile.mkdtemp(prefix="tomos-sound-"), "out.wav")
    vm = boot_test.VM(port, gui=False, qemu_args=[
        "-audiodev", "wav,id=snd,path=" + wav_path, "-device", "AC97,audiodev=snd"])
    steps = 0

    def step(name, fn):
        nonlocal steps
        fn()
        steps += 1
        print("ok %2d - %s" % (steps, name))

    try:
        step("the sound card is found", lambda: (
            vm.expect("network up", 90), vm.expect(PROMPT),
            vm.command("/lua print(sound.available())", "true", PROMPT),
            vm.command("/info", "sound  : AC'97", PROMPT)))

        step("tones, a melody, a chord, a stopped tone and samples play", lambda: (
            vm.command("/lua " + SCRIPT, "played", PROMPT, timeout=60)))

        def halt():
            vm.send("/halt\n")
            vm.proc.wait(timeout=15)
        step("powers off (QEMU finishes the recording)", halt)

        w = wave.open(wav_path)
        rate, ch, n = w.getframerate(), w.getnchannels(), w.getnframes()
        data = struct.unpack("<%dh" % (n * ch), w.readframes(n))
        left = data[::ch]
        segs = segments(left, rate)
        lengths = ["%.2f" % ((b - a) / rate) for a, b in segs]
        assert len(segs) == 5, "expected 5 sounds, heard %d: %s" % (len(segs), lengths)

        def check_tone():
            a, b = segs[0]
            mid = left[a + (b - a) // 4: b - (b - a) // 4]
            f = pitch(mid, rate)
            assert abs(f - 440) < 10, "pitch %.1f Hz" % f
            assert 0.5 < (b - a) / rate < 0.75, "length %.2f s" % ((b - a) / rate)
        step("a 440 Hz sine for 0.6 s", check_tone)

        def check_melody():
            a, b = segs[1]
            third = (b - a) // 3
            got = [pitch(left[a + i * third + third // 4: a + (i + 1) * third - third // 4], rate) for i in range(3)]
            for f, want in zip(got, (523.3, 659.3, 784.0)):
                assert abs(f - want) < want * 0.03, "melody pitches %s" % ["%.0f" % g for g in got]
        step("the melody C5 E5 G5 in order", check_melody)

        def check_chord():
            a, b = segs[2]
            x = left[a + (b - a) // 4: b - (b - a) // 4][:8000]
            e440, e660, e550 = (goertzel(x, rate, f) for f in (440, 660, 550))
            assert e440 > 10 * e550 and e660 > 10 * e550, "chord energies %.0f %.0f %.0f" % (e440, e660, e550)
        step("two tones at once are mixed (440 Hz + 660 Hz)", check_chord)

        def check_stop():
            a, b = segs[3]
            assert (b - a) / rate < 0.6, "sound.stop() ended a 5 s tone after %.2f s" % ((b - a) / rate)
        step("sound.stop() silences a long tone", check_stop)

        def check_samples():
            a, b = segs[4]
            f = pitch(left[a + (b - a) // 4: b - (b - a) // 4], rate)
            assert abs(f - 1000) < 30, "sample playback pitch %.0f Hz" % f
            assert 0.4 < (b - a) / rate < 0.65, "0.5 s of samples took %.2f s" % ((b - a) / rate)
        step("a table of samples plays at its own rate (1 kHz at 22050 Hz)", check_samples)

        def no_card():
            vm2 = boot_test.VM(port, gui=False)
            vm2.expect(PROMPT, 90)
            vm2.command("/lua print(sound.available(), sound.tone(440, 100))", "false\tfalse", PROMPT)
            vm2.proc.kill()
        step("without a sound card, sound calls do nothing and say so", no_card)
    except Exception as e:
        print("FAILED at step %d: %s" % (steps + 1, e))
        vm.proc.kill()
        return 1
    print("all %d steps passed" % steps)
    return 0


if __name__ == "__main__":
    sys.exit(main())
