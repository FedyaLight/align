#!/usr/bin/env python3
"""Regenerate the blank AAF metadictionary using the pinned pyaaf2 build environment."""
import datetime
import gzip
from tempfile import TemporaryDirectory
from pathlib import Path
from uuid import UUID

import aaf2


def main():
    # No project data. Give the two dictionary weak references stable indexes.
    with TemporaryDirectory() as directory:
        path = Path(directory) / 'dictionary.aaf'
        generate(path)
        data = path.read_bytes()
    output = Path(__file__).resolve().parents[1] / 'crates/align-aaf/dictionary.aaf.gz'
    output.write_bytes(gzip.compress(data, mtime=0))


def generate(path):
    with aaf2.open(path, 'w') as container:
        assert container.weakref_index([2, 0x3b04, 0x2605]) == 2
        assert container.weakref_index([2, 0x3b04, 0x2608]) == 3
        timestamp = datetime.datetime(2000, 1, 1)
        container.header['LastModified'].value = timestamp
        item = container.header['IdentificationList'][0]
        item['CompanyName'].value = 'Align'
        item['ProductName'].value = 'Align'
        item['ProductID'].value = UUID('7cb023e7-4730-4c4c-8747-642443d1337b')
        item['GenerationAUID'].value = UUID(int=0)
        item['Date'].value = timestamp
        item['ProductVersionString'].value = 'Align'
        item['Platform'].value = 'Align'


if __name__ == '__main__':
    main()
