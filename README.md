# oscilloscope-me

FM SDR receiver with a terminal **X/Y vectorscope** for [oscilloscope music](https://oscilloscopemusic.com/). Tune an FM station (e.g. ToorCamp's LOL radio), preview Lissajous shapes in the terminal, and output stereo L/R audio for an analog oscilloscope in XY mode.

**Left = X, Right = Y**

## Hardware

- **SDR:** RTL-SDR compatible dongle (tested target: NooElec NESDR Smart v5 — RTL2832U + R820T2/R860)
- **Antenna:** FM band antenna
- **Optional:** Analog oscilloscope in XY mode, shielded stereo audio cable (192 kHz capable)
- **Note:** Laptop headphone jacks are AC-coupled; images may drift on an analog scope. A DC-coupled USB DAC gives better results.

### Scope wiring

1. Set oscilloscope to **XY mode**
2. **Left audio → X input**
3. **Right audio → Y input**

## Install

### macOS (Apple Silicon)

```bash
brew install libusb pkg-config
cargo build --release
```

### Linux

```bash
# Debian/Ubuntu
sudo apt install libusb-1.0-0-dev pkg-config

# Fedora
sudo dnf install libusb1-devel pkg-config

cargo build --release
```

#### USB permissions (Linux)

```bash
sudo cp udev/99-rtlsdr.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules
sudo udevadm trigger
sudo usermod -aG plugdev "$USER"
# log out and back in
```

If you see `Usb(Busy)`, the kernel DVB driver may be claiming the dongle:

```bash
sudo rmmod rtl2832_sdr dvb_usb_rtl28xxu rtl2832 rtl8xxxu
```

For a permanent fix, blacklist those modules (see [rtl-sdr-rs Linux notes](https://github.com/ccostes/rtl-sdr-rs#linux-kernel-modules)).

## Usage

```bash
cargo run --release
```

**If you hear static**, try the proven mono demod path first (same algorithm as `rtl_fm`):

```bash
cargo run --release -- --mono -f 94.1 -r 48000
```

Stereo / scope mode (higher sample rate for X/Y output):

```bash
cargo run --release -- -f 94.1 -r 192000
```

1. Plug in the SDR — the app waits until one is detected
2. Tunes **94.1 MHz** by default (override with `-f`)
3. Terminal shows a live X/Y vectorscope; audio plays on the default output device

### Options

```
oscilloscope-me [OPTIONS]

  -f, --freq <MHZ>           FM frequency in MHz
  -g, --gain <DB|auto>       Tuner gain (default: auto)
      --mono                 Force mono decode (skip stereo PLL)
  -a, --audio-device <NAME>  Output device name substring
  -r, --sample-rate <HZ>     Target output rate (default: 192000)
      --ppm <PPM>            Frequency correction (default: 0)
```

### In-app keys

| Key | Action |
|-----|--------|
| `q` / `Esc` | Quit |
| `+` / `=` | Tune +0.1 MHz |
| `-` | Tune −0.1 MHz |
| `g` | Cycle gain (auto → 0 → 20 → 40 dB) |

## How it works

```
RTL-SDR IQ → FM quadrature demod → stereo MPX decode (19 kHz pilot)
          → de-emphasis → L/R audio (cpal) + terminal vectorscope
```

Internal MPX processing runs at **192 kHz**; audio is resampled to the best rate your output device supports (192 kHz preferred).

## License

GPL-3.0-or-later. Uses [rtl-sdr-rs](https://github.com/ccostes/rtl-sdr-rs) (MPL-2.0).
