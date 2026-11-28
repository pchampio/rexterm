"""prefix a T: connect other Rex apps to this Rex server over Tailscale.

The popup runs on the machine of the Rex server, with the server's own
`rex` command (REX_BIN_DIR, REX_SERVER). It asks the server for the status of
its built-in Tailscale every second and shows:

- off: Enter turns it on (only your Tailscale account may connect);
- signing in: the sign-in link, as a QR code, a link and a Copy button;
- on the tailnet: the server's address (https://NAME.TAILNET.ts.net), as a
  QR code, a link and a Copy button, for any Rex app on a phone or another
  computer of the tailnet.

Copy uses OSC 52, so the text goes to the clipboard of the computer that
shows the popup. Keys: c copy, x turn Tailscale off (twice), q or Escape
close. A click on [ Copy ] copies too.

argv[1], when set, is the `rex` command to use (the tests give a stand-in).
"""
import base64
import json
import os
import select
import subprocess
import sys
import termios
import textwrap
import time
import tty

# ---------------------------------------------------------------- QR code
# Byte mode, error correction level M, versions 1 to 10 (up to 213 bytes):
# enough for a URL. Written after the QR Code specification (ISO/IEC 18004).

# Version: (error correction codewords per block, [data codewords per block]).
_BLOCKS = {
    1: (10, [16]), 2: (16, [28]), 3: (26, [44]), 4: (18, [32, 32]),
    5: (24, [43, 43]), 6: (16, [27] * 4), 7: (18, [31] * 4),
    8: (22, [38, 38, 39, 39]), 9: (22, [36, 36, 36, 37, 37]),
    10: (26, [43, 43, 43, 43, 44]),
}
_ALIGN = {1: [], 2: [6, 18], 3: [6, 22], 4: [6, 26], 5: [6, 30], 6: [6, 34],
          7: [6, 22, 38], 8: [6, 24, 42], 9: [6, 26, 46], 10: [6, 28, 50]}


def _gf_mul(a, b):
    r = 0
    for i in range(7, -1, -1):
        r = (r << 1) ^ ((r >> 7) * 0x11D)
        r ^= ((b >> i) & 1) * a
    return r & 0xFF


def _rs_remainder(data, degree):
    divisor = [0] * (degree - 1) + [1]
    root = 1
    for _ in range(degree):
        for j in range(degree):
            divisor[j] = _gf_mul(divisor[j], root)
            if j + 1 < degree:
                divisor[j] ^= divisor[j + 1]
        root = _gf_mul(root, 2)
    result = [0] * degree
    for b in data:
        factor = b ^ result.pop(0)
        result.append(0)
        for i, coef in enumerate(divisor):
            result[i] ^= _gf_mul(coef, factor)
    return result


def qr_matrix(text):
    """The modules of a QR code for TEXT: rows of booleans, True is dark."""
    data = text.encode('utf-8')
    for version in range(1, 11):
        ec_len, blocks = _BLOCKS[version]
        count_bits = 8 if version < 10 else 16
        if 4 + count_bits + 8 * len(data) <= 8 * sum(blocks):
            break
    else:
        raise ValueError('text too long for a QR code')
    capacity = 8 * sum(blocks)
    bits = [0, 1, 0, 0]
    bits += [(len(data) >> i) & 1 for i in range(count_bits - 1, -1, -1)]
    for b in data:
        bits += [(b >> i) & 1 for i in range(7, -1, -1)]
    bits += [0] * min(4, capacity - len(bits))
    bits += [0] * (-len(bits) % 8)
    codewords = [int(''.join(map(str, bits[i:i + 8])), 2) for i in range(0, len(bits), 8)]
    pad = 0xEC
    while len(codewords) < sum(blocks):
        codewords.append(pad)
        pad ^= 0xEC ^ 0x11
    # Split into blocks, add error correction, interleave.
    parts, start = [], 0
    for n in blocks:
        parts.append(codewords[start:start + n])
        start += n
    eccs = [_rs_remainder(p, ec_len) for p in parts]
    stream = []
    for i in range(max(blocks)):
        stream += [p[i] for p in parts if i < len(p)]
    for i in range(ec_len):
        stream += [e[i] for e in eccs]
    size = 17 + 4 * version
    modules = [[False] * size for _ in range(size)]
    function = [[False] * size for _ in range(size)]

    def put(x, y, dark):
        modules[y][x] = dark
        function[y][x] = True

    for i in range(size):
        put(6, i, i % 2 == 0)
        put(i, 6, i % 2 == 0)
    for cx, cy in ((3, 3), (size - 4, 3), (3, size - 4)):
        for dy in range(-4, 5):
            for dx in range(-4, 5):
                if 0 <= cx + dx < size and 0 <= cy + dy < size:
                    put(cx + dx, cy + dy, max(abs(dx), abs(dy)) not in (2, 4))
    align = _ALIGN[version]
    for i, ay in enumerate(align):
        for j, ax in enumerate(align):
            if (i, j) in ((0, 0), (0, len(align) - 1), (len(align) - 1, 0)):
                continue
            for dy in range(-2, 3):
                for dx in range(-2, 3):
                    put(ax + dx, ay + dy, max(abs(dx), abs(dy)) != 1)

    def format_bits(mask):
        value = (0 << 3) | mask  # level M is 00
        rem = value
        for _ in range(10):
            rem = (rem << 1) ^ ((rem >> 9) * 0x537)
        return ((value << 10) | rem) ^ 0x5412

    def draw_format(mask):
        f = format_bits(mask)
        bit = lambda i: (f >> i) & 1 == 1
        for i in range(6):
            put(8, i, bit(i))
        put(8, 7, bit(6))
        put(8, 8, bit(7))
        put(7, 8, bit(8))
        for i in range(9, 15):
            put(14 - i, 8, bit(i))
        for i in range(8):
            put(size - 1 - i, 8, bit(i))
        for i in range(8, 15):
            put(8, size - 15 + i, bit(i))
        put(8, size - 8, True)

    draw_format(0)  # reserves the format areas
    if version >= 7:
        rem = version
        for _ in range(12):
            rem = (rem << 1) ^ ((rem >> 11) * 0x1F25)
        v = (version << 12) | rem
        for i in range(18):
            dark = (v >> i) & 1 == 1
            a, b = size - 11 + i % 3, i // 3
            put(a, b, dark)
            put(b, a, dark)
    # The data, in two-column zigzags from the bottom right.
    data_bits = [(cw >> i) & 1 for cw in stream for i in range(7, -1, -1)]
    k = 0
    right = size - 1
    while right >= 1:
        if right == 6:
            right = 5
        for vert in range(size):
            for j in range(2):
                x = right - j
                upward = ((right + 1) & 2) == 0
                y = size - 1 - vert if upward else vert
                if not function[y][x] and k < len(data_bits):
                    modules[y][x] = data_bits[k] == 1
                    k += 1
        right -= 2

    masks = [
        lambda x, y: (x + y) % 2 == 0, lambda x, y: y % 2 == 0,
        lambda x, y: x % 3 == 0, lambda x, y: (x + y) % 3 == 0,
        lambda x, y: (x // 3 + y // 2) % 2 == 0, lambda x, y: x * y % 2 + x * y % 3 == 0,
        lambda x, y: (x * y % 2 + x * y % 3) % 2 == 0, lambda x, y: ((x + y) % 2 + x * y % 3) % 2 == 0,
    ]

    def masked(mask):
        draw_format(mask)
        out = [row[:] for row in modules]
        for y in range(size):
            for x in range(size):
                if not function[y][x] and masks[mask](x, y):
                    out[y][x] = not out[y][x]
        return out

    best = min(range(8), key=lambda m: _penalty(masked(m)))
    return masked(best)


def _penalty(m):
    size = len(m)
    score = 0
    lines = [row for row in m] + [[m[y][x] for y in range(size)] for x in range(size)]
    finder = [True, False, True, True, True, False, True]
    for line in lines:
        run, last = 0, None
        for v in line:
            if v == last:
                run += 1
            else:
                if run >= 5:
                    score += 3 + run - 5
                run, last = 1, v
        if run >= 5:
            score += 3 + run - 5
        padded = [False] * 4 + list(line) + [False] * 4
        for i in range(len(padded) - 6):
            if padded[i:i + 7] == finder and (padded[max(0, i - 4):i] == [False] * 4 or padded[i + 7:i + 11] == [False] * 4):
                score += 40
    for y in range(size - 1):
        for x in range(size - 1):
            if m[y][x] == m[y][x + 1] == m[y + 1][x] == m[y + 1][x + 1]:
                score += 3
    dark = sum(v for row in m for v in row)
    score += 10 * (abs(dark * 100 // (size * size) - 50) // 5)
    return score


def qr_lines(text, quiet=2):
    """The QR code as terminal lines: two modules per line with half blocks,
    black on white whatever the colour scheme."""
    m = qr_matrix(text)
    size = len(m) + 2 * quiet
    grid = [[False] * size for _ in range(quiet)]
    grid += [[False] * quiet + row + [False] * quiet for row in m]
    grid += [[False] * size for _ in range(quiet + (size % 2))]
    colour = lambda dark: '0;0;0' if dark else '255;255;255'
    lines = []
    for y in range(0, len(grid) - 1, 2):
        cells = []
        for x in range(size):
            cells.append('\x1b[38;2;%sm\x1b[48;2;%sm▀' % (colour(grid[y][x]), colour(grid[y + 1][x])))
        lines.append(''.join(cells) + '\x1b[0m')
    return lines, size


# ------------------------------------------------------------------ server
REX = (sys.argv[1] if len(sys.argv) > 1 and sys.argv[1] else
       os.path.join(os.environ['REX_BIN_DIR'], 'rex') if os.environ.get('REX_BIN_DIR') else 'rex')


def call(method, args=None):
    command = [REX]
    if os.environ.get('REX_SERVER'):
        command += ['-S', os.environ['REX_SERVER']]
    command += ['--autostart=false', 'api', 'call', method, json.dumps(args or {})]
    done = subprocess.run(command, capture_output=True, text=True, timeout=20)
    if done.returncode != 0:
        message = (done.stderr or done.stdout).strip().splitlines()
        raise RuntimeError(message[-1] if message else '%s failed' % method)
    return json.loads(done.stdout or '{}')


# --------------------------------------------------------------------- UI
DIM, BOLD, RESET = '\x1b[2m', '\x1b[1m', '\x1b[0m'
GREEN, YELLOW, RED, BLUE = '\x1b[32m', '\x1b[33m', '\x1b[31m', '\x1b[34m'


def link(url, text=None):
    """TEXT (the URL itself by default) as a link to URL (OSC 8)."""
    return '\x1b]8;;%s\x1b\\%s%s%s\x1b]8;;\x1b\\' % (url, BLUE, text or url, RESET)


class Popup:
    def __init__(self):
        self.status = None
        self.error = ''
        self.note = ''
        self.note_until = 0
        self.confirm_off = 0
        self.login_asked = False
        self.copy_text = ''
        self.button = None  # (row, first column, last column), 1-based
        self.out = sys.stdout

    def refresh(self):
        before = (self.status or {}).get('state')
        try:
            self.status = call('server.status').get('tailscale') or {'state': 'disabled'}
            self.error = ''
        except Exception as error:  # Shown on screen; the next refresh tries again.
            self.error = str(error)
        s = self.status or {}
        if s.get('state') != before:
            self.note_until = 0  # A note such as "Starting…" is about the old state.
        if s.get('state') == 'needs_login' and not s.get('auth_url') and not self.login_asked:
            self.login_asked = True
            try:
                self.status = call('server.tailscale.login').get('tailscale') or s
            except Exception as error:
                self.error = str(error)

    def say(self, text):
        self.note, self.note_until = text, time.time() + 3

    def enable(self):
        try:
            self.status = call('server.tailscale.enable', {'allow_tailnet_members': False}).get('tailscale') or self.status
            self.say('Starting Tailscale…')
        except Exception as error:
            self.error = str(error)

    def disable(self):
        try:
            self.status = call('server.tailscale.disable').get('tailscale') or self.status
            self.say('Tailscale is off')
        except Exception as error:
            self.error = str(error)

    def copy(self):
        if not self.copy_text:
            return
        data = base64.b64encode(self.copy_text.encode()).decode()
        self.out.write('\x1b]52;c;%s\x07' % data)
        self.say('Copied to the clipboard')

    def draw(self):
        cols, rows = os.get_terminal_size()
        width = max(10, cols - 2)
        s = self.status or {}
        state = s.get('state') or ('disabled' if self.status is not None else 'starting')
        https = s.get('https') or {}
        account = (s.get('account') or {}).get('login_name')
        lines = []

        def para(text, style=''):
            """TEXT wrapped to the popup's width, one space in from the left:
            a line wider than the popup would wrap and scroll the screen."""
            for line in textwrap.wrap(text, width) or ['']:
                lines.append(' ' + (style + line + RESET if style else line))

        para('Tailscale connect', BOLD)
        lines.append('')
        qr_text = ''
        self.copy_text = ''
        if self.status is None and self.error:
            para('The Rex server does not answer:', RED)
            para(self.error)
        elif state == 'disabled':
            para('Built-in Tailscale is off.')
            lines.append('')
            para('Press Enter to turn it on: the server joins your tailnet as a device of its own. '
                 'Only your Tailscale account may connect.')
            if s.get('error'):
                lines.append('')
                para('It turned off: ' + s['error'], RED)
        elif state == 'needs_login':
            url = s.get('auth_url', '')
            para('Sign the server in to Tailscale', YELLOW)
            para('Open the link: scan it with your phone, or copy it.')
            if url:
                qr_text = self.copy_text = url
            else:
                lines.append('')
                para('Waiting for the sign-in link…', DIM)
        elif state == 'needs_machine_approval':
            para('A tailnet admin must approve this device', YELLOW)
            para('in the Tailscale admin console (Machines).')
        elif state == 'running' and https.get('state') == 'available' and https.get('url'):
            url = https['url']
            qr_text = self.copy_text = url
            who = ('anyone your tailnet\'s access rules allow' if s.get('allow_tailnet_members')
                   else 'only ' + (account or 'the Tailscale account that signed in'))
            para('On the tailnet' + (' ' + s['tailnet_name'] if s.get('tailnet_name') else ''), GREEN)
            para('Rex apps on your phone or other computers connect to this address. '
                 'Who may connect: ' + who + '.')
        elif state == 'running':
            para('On the tailnet. Waiting for the HTTPS certificate…')
        else:
            para('Starting Tailscale…', DIM)
        if self.error and self.status is not None:
            lines.append('')
            para(self.error, RED)
        self.button = None
        if qr_text:
            lines.append('')
            qr, qr_width = qr_lines(qr_text)
            room = rows - len(lines) - 5 - (len(qr_text) // width)
            if len(qr) <= room and qr_width <= cols:
                pad = ' ' * max(0, (cols - qr_width) // 2)
                lines += [pad + line for line in qr]
            else:
                para('A larger popup shows the QR code: prefix Space zooms it.', DIM)
            lines.append('')
            # A long link goes on several lines, each part a link to all of it.
            for i in range(0, len(qr_text), width):
                lines.append(' ' + link(qr_text, qr_text[i:i + width]))
            lines.append('')
            label = '[ Copy ]'
            self.button = (len(lines) + 1, 2, 1 + len(label))
            note = self.note if time.time() < self.note_until else ''
            lines.append(' ' + BOLD + '\x1b[7m' + label + RESET + '  ' + GREEN + note[:max(0, width - len(label) - 2)] + RESET)
        elif time.time() < self.note_until:
            lines.append('')
            para(self.note, GREEN)
        keys = ['c: copy'] if qr_text else []
        if state == 'disabled':
            keys.insert(0, 'Enter: turn on')
        elif self.status is not None:
            keys.append('x twice: turn off' if time.time() > self.confirm_off else 'x again: turn off')
        keys.append('q: close')
        footer = ' ' + '  ·  '.join(keys)
        if len(footer) > cols:
            footer = ' ' + ' · '.join(keys)
        lines = lines[:rows - 1]
        lines += [''] * (rows - 1 - len(lines))
        lines.append(DIM + footer[:cols] + RESET)
        self.out.write('\x1b[H' + ''.join(line + '\x1b[K\r\n' for line in lines[:-1]) + lines[-1] + '\x1b[K')
        self.out.flush()

    def key(self, data):
        """Handle input; False to close."""
        i = 0
        while i < len(data):
            if data.startswith('\x1b[<', i):
                end = min([j for j in (data.find('M', i), data.find('m', i)) if j >= 0] or [len(data)])
                parts = data[i + 3:end].split(';')
                if data[end:end + 1] == 'M' and len(parts) == 3 and parts[0] == '0' and self.button:
                    x, y = int(parts[1]), int(parts[2])
                    row, first, last = self.button
                    if y == row and first <= x <= last:
                        self.copy()
                i = end + 1
                continue
            if data[i] == '\x1b' and data.startswith('\x1b[', i):
                i += 3
                continue
            c = data[i]
            i += 1
            state = (self.status or {}).get('state')
            if c in ('q', '\x1b', '\x03'):
                return False
            if c in ('\r', '\n') and state == 'disabled':
                self.enable()
            elif c == 'c':
                self.copy()
            elif c == 'x' and state not in (None, 'disabled'):
                if time.time() < self.confirm_off:
                    self.confirm_off = 0
                    self.disable()
                else:
                    self.confirm_off = time.time() + 3
        return True


def main():
    fd = sys.stdin.fileno()
    saved = termios.tcgetattr(fd)
    popup = Popup()
    out = sys.stdout
    try:
        tty.setcbreak(fd)
        out.write('\x1b[?1049h\x1b[?25l\x1b[?1000h\x1b[?1006h\x1b[2J')
        last = 0
        while True:
            if time.time() - last >= 1:
                popup.refresh()
                last = time.time()
            popup.draw()
            ready, _, _ = select.select([fd], [], [], 0.25)
            if ready:
                data = os.read(fd, 4096).decode(errors='replace')
                if not data or not popup.key(data):
                    break
    finally:
        out.write('\x1b[?1006l\x1b[?1000l\x1b[?25h\x1b[?1049l')
        out.flush()
        termios.tcsetattr(fd, termios.TCSADRAIN, saved)


if __name__ == '__main__':
    main()
