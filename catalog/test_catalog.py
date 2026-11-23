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


class LicenseUrls(unittest.TestCase):
    def test_arcgis_signature_becomes_a_public_item_link(self):
        item = "a" * 32
        url = f"https://ago-item-storage.s3.us-east-1.amazonaws.com/{item}/license.pdf?X-Amz-Signature=test"
        self.assertEqual(catalog.trim_license({"url": url}),
                         {"url": f"https://www.arcgis.com/sharing/rest/content/items/{item}/data"})

    def test_a_signed_url_retains_only_its_non_signing_parameters(self):
        url = "https://license.example/file?version=2&X-Amz-Credential=test&x-amz-signature=test"
        self.assertEqual(catalog.public_license_url(url), "https://license.example/file?version=2")

    def test_a_public_url_is_preserved(self):
        url = "https://license.example/item?id=12#terms"
        self.assertEqual(catalog.public_license_url(url), url)


if __name__ == "__main__":
    unittest.main()
