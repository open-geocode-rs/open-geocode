"""Offline tests for the parts of catalog.py that read Geofabrik's index."""

import unittest

import catalog


def link(folder, name):
    return f"https://download.geofabrik.de/{folder}/{name}-latest.osm.pbf"


class IsoCodes(unittest.TestCase):
    def test_a_list_is_read_as_it_is(self):
        self.assertEqual(catalog.iso_codes(["HT", "DO"]), ["HT", "DO"])

    def test_codes_joined_by_commas_are_split(self):
        self.assertEqual(catalog.iso_codes("HT,DO"), ["HT", "DO"])

    def test_one_code_is_a_list_of_one(self):
        self.assertEqual(catalog.iso_codes("MC"), ["MC"])


class MovedLinks(unittest.TestCase):
    OLD = {
        "monaco": link("europe/monaco", "monaco"),
        "malta": link("europe/malta", "malta"),
        "sealand": link("europe/sealand", "sealand"),
    }
    NEW = {
        "monaco": link("europe/monaco", "monaco"),
        "malta": link("europe/islands/malta", "malta"),
        "ashmore-cartier": link("australia-oceania/ashmore-cartier", "ashmore-cartier"),
    }

    def test_exactly_the_moved_added_and_removed_regions_are_listed(self):
        self.assertEqual(
            catalog.moved_links(self.OLD, self.NEW),
            [
                f"ashmore-cartier: not pinned -> {self.NEW['ashmore-cartier']}",
                f"malta: {self.OLD['malta']} -> {self.NEW['malta']}",
                f"sealand: {self.OLD['sealand']} -> gone",
            ],
        )

    def test_nothing_is_listed_when_no_link_changed(self):
        self.assertEqual(catalog.moved_links(self.OLD, dict(self.OLD)), [])


if __name__ == "__main__":
    unittest.main()
