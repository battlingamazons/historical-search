"""Minimal reader for Visual FoxPro .DBF/.FPT tables, as used by PastPerfect.

No external dependencies. Reads field metadata from the DBF header, then
yields each non-deleted record as a dict of Python values, resolving memo
(type M) fields against the companion .FPT file.
"""
import struct
import datetime
import os


class Memo:
    def __init__(self, fpt_path):
        self.f = open(fpt_path, 'rb')
        header = self.f.read(512)
        self.block_size = struct.unpack('>H', header[6:8])[0] or 512

    def get(self, block_num):
        if not block_num:
            return ''
        self.f.seek(block_num * self.block_size)
        block_header = self.f.read(8)
        if len(block_header) < 8:
            return ''
        _mtype, length = struct.unpack('>II', block_header)
        if length <= 0 or length > 10_000_000:
            return ''
        return self.f.read(length).decode('latin1', errors='replace')

    def close(self):
        self.f.close()


class DBFTable:
    def __init__(self, dbf_path):
        self.dbf_path = dbf_path
        base, _ = os.path.splitext(dbf_path)
        fpt_path = None
        for ext in ('.fpt', '.FPT'):
            candidate = base + ext
            if os.path.exists(candidate):
                fpt_path = candidate
                break
        self.memo = Memo(fpt_path) if fpt_path else None

        with open(dbf_path, 'rb') as f:
            self.data = f.read()

        self.num_records = struct.unpack('<I', self.data[4:8])[0]
        self.header_len = struct.unpack('<H', self.data[8:10])[0]
        self.record_len = struct.unpack('<H', self.data[10:12])[0]

        self.fields = []
        pos = 32
        while self.data[pos] != 0x0D:
            name = self.data[pos:pos + 11].split(b'\x00')[0].decode('latin1')
            ftype = chr(self.data[pos + 11])
            flen = self.data[pos + 16]
            self.fields.append((name, ftype, flen))
            pos += 32

        self._offsets = {}
        off = 1  # byte 0 of each record is the deletion flag
        for name, ftype, flen in self.fields:
            self._offsets[name] = (off, flen, ftype)
            off += flen

    def field_names(self):
        return [name for name, _, _ in self.fields]

    def _coerce(self, name, raw):
        off, flen, ftype = self._offsets[name]
        if ftype == 'M':
            block_txt = raw.strip()
            if not block_txt:
                return ''
            try:
                block_num = int.from_bytes(raw, 'little')
                # Fall back to ASCII-digit pointer if binary looks bogus.
                if block_num == 0 and block_txt.strip(b'\x00').isdigit():
                    block_num = int(block_txt)
            except Exception:
                return ''
            return self.memo.get(block_num) if self.memo else ''
        text = raw.decode('latin1', errors='replace')
        if ftype == 'D':
            text = text.strip()
            if len(text) == 8 and text.isdigit():
                try:
                    return datetime.date(int(text[0:4]), int(text[4:6]), int(text[6:8])).isoformat()
                except ValueError:
                    return ''
            return ''
        if ftype == 'N':
            text = text.strip()
            if not text:
                return None
            try:
                return float(text) if '.' in text else int(text)
            except ValueError:
                return None
        if ftype == 'L':
            return text.strip().upper() in ('T', 'Y')
        return text.rstrip()

    def records(self):
        for i in range(self.num_records):
            start = self.header_len + i * self.record_len
            rec = self.data[start:start + self.record_len]
            if not rec or rec[0:1] == b'*':
                continue  # soft-deleted record
            out = {}
            for name, ftype, flen in self.fields:
                off, flen2, ftype2 = self._offsets[name]
                raw = rec[off:off + flen2]
                out[name] = self._coerce(name, raw)
            yield out

    def close(self):
        if self.memo:
            self.memo.close()
