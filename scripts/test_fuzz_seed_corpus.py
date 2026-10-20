#!/usr/bin/env python3
"""Validate the positive metainfo seeds rather than only exercising rejection."""
import unittest
import fuzz_seed_corpus as corpus


def decode(data):
    def value(offset):
        marker = data[offset:offset + 1]
        if marker == b'i':
            end = data.index(b'e', offset)
            return int(data[offset + 1:end]), end + 1
        if marker in (b'l', b'd'):
            result = []
            offset += 1
            while data[offset:offset + 1] != b'e':
                item, offset = value(offset)
                result.append(item)
            if marker == b'd':
                if len(result) % 2:
                    raise ValueError('unpaired dictionary entry')
                result = dict(zip(result[::2], result[1::2]))
            return result, offset + 1
        colon = data.index(b':', offset)
        length = int(data[offset:colon])
        end = colon + 1 + length
        if end > len(data):
            raise ValueError('truncated string')
        return data[colon + 1:end], end
    result, end = value(0)
    if end != len(data):
        raise ValueError('trailing bytes')
    return result


class SeedTests(unittest.TestCase):
    def test_positive_metainfo_is_complete_and_matches_info_seed(self):
        corpus.refresh_seeds()
        for kind in ('v1', 'multi', 'v2', 'hybrid'):
            with self.subTest(kind=kind):
                info = decode(corpus.SEEDS['torrent_info_parse']['valid_' + kind])
                envelope = decode(corpus.SEEDS['torrent_file_parse']['valid_' + kind])
                self.assertEqual(envelope[b'info'], info)
                self.assertEqual(info[b'piece length'], 16384)
                if kind in ('v2', 'hybrid'):
                    self.assertEqual(info[b'meta version'], 2)
                    leaf = info[b'file tree'][b'orbital.bin'][b'']
                    self.assertEqual(len(leaf[b'pieces root']), 32)
                if kind == 'multi':
                    self.assertEqual(info[b'files'][0][b'path'], [b'part.bin'])
        self.assertEqual(decode(corpus.SEEDS['torrent_info_parse']['info_files']),
                         decode(corpus.SEEDS['torrent_info_parse']['valid_multi']))

    def test_compact_ipv6_records_have_complete_widths(self):
        corpus.refresh_seeds()
        self.assertEqual(len(corpus.SEEDS['krpc_compact_decode']['ipv6_peer']), 1 + 18)
        self.assertEqual(len(corpus.SEEDS['krpc_compact_decode']['ipv6_node']), 1 + 38)


if __name__ == '__main__':
    unittest.main()
