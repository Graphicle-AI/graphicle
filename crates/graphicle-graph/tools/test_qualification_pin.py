"""Qualification must reject a binary/reference pin mismatch before comparisons."""
import unittest
from parity import validate_upstream


def info(commit, version, modules):
    """The native `info` shape: the ported release is attribution under `based_on`."""
    return {'implementation': 'rust',
            'based_on': {'upstream_commit': commit, 'upstream_version': version,
                         'complete_port': True, 'application_modules': modules}}


class QualificationPin(unittest.TestCase):
    def test_old_inventory_cannot_qualify_new_release(self):
        ledger = {'upstream_commit': 'old', 'upstream_version': '0.9.72',
                  'files': [{'category': 'application_python'}]}
        with self.assertRaisesRegex(ValueError, 'upstream_commit differs'):
            validate_upstream(info('new', '0.9.73', 1), ledger)

    def test_matching_pin_also_requires_correct_module_count(self):
        ledger = {'upstream_commit': 'new', 'upstream_version': '0.9.73',
                  'files': [{'category': 'application_python'}, {'category': 'upstream_test_python'}]}
        current = info('new', '0.9.73', 1)
        validate_upstream(current, ledger)
        current['based_on']['application_modules'] = 87
        with self.assertRaisesRegex(ValueError, 'module count differs'):
            validate_upstream(current, ledger)

    def test_pin_fields_are_read_only_from_based_on(self):
        ledger = {'upstream_commit': 'new', 'upstream_version': '0.9.73',
                  'files': [{'category': 'application_python'}]}
        flat = {'upstream_commit': 'new', 'upstream_version': '0.9.73', 'application_modules': 1}
        with self.assertRaisesRegex(ValueError, 'upstream_commit differs'):
            validate_upstream(flat, ledger)


if __name__ == '__main__':
    unittest.main()
