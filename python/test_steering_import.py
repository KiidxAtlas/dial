"""Steering pack import: rebinding to the local checkout and refusing malformed bundles."""
import json
import tempfile
import unittest
from pathlib import Path

import dial_steering as s


def bundle():
    result = dict(version=1, model_name='exporter', model='a' * 64, adapter=None, layer=0, layer_count=2, dimension=2, vector=[1., 0.], training_prompts=[s.digest([n]) for n in range(8)], pairs=8)
    result['id'] = s.digest(s.payload(result))
    result['evaluation'] = dict(passed=True, artifact=result['id'], strength=.5, cases=8, baseline_passes=4, steered_passes=7, scope='held-out response contracts', tool_nonregression=True, repetition_nonregression=True, latency_nonregression=True)
    return result


class ImportBundle(unittest.TestCase):
    def test_rebinds_identity_and_drops_evaluation(self):
        with tempfile.TemporaryDirectory() as tmp:
            model = Path(tmp) / 'model'
            model.mkdir()
            (model / 'config.json').write_text('{}')
            (model / 'model.safetensors').write_bytes(b'w')
            output = Path(tmp) / 'bound.json'
            output.write_text(json.dumps(bundle()))

            result = s.import_bundle({'op': 'steering_import', 'model': str(model), 'model_name': 'local-model', 'adapter': None, 'output': str(output)})

            self.assertEqual(result, {'bound': True, 'pairs': 8, 'layer': 0})
            written = json.loads(output.read_text())
            self.assertNotIn('evaluation', written)
            self.assertEqual(written['model'], s.identity(str(model)))
            self.assertIsNone(written['adapter'])
            self.assertEqual(written['model_name'], 'local-model')
            self.assertEqual(written['id'], s.digest(s.payload(written)))
            s.validate(written)

    def test_malformed_bundle_is_rejected_before_rebinding(self):
        with tempfile.TemporaryDirectory() as tmp:
            model = Path(tmp) / 'model'
            model.mkdir()
            (model / 'config.json').write_text('{}')
            broken = bundle()
            broken['vector'] = [2., 0.]
            output = Path(tmp) / 'bound.json'
            output.write_text(json.dumps(broken))

            with self.assertRaises(ValueError):
                s.import_bundle({'op': 'steering_import', 'model': str(model), 'model_name': 'local', 'adapter': None, 'output': str(output)})
            self.assertEqual(json.loads(output.read_text()), broken)


if __name__ == '__main__':
    unittest.main()
