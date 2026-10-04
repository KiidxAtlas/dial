import json
import unittest
from dial_server import ToolCallFormatter


class FormatterTests(unittest.TestCase):
    def format(self, text, streaming=False):
        return ToolCallFormatter(lambda text, tools: json.loads(text), [], streaming)([text])

    def test_missing_arguments_are_explicitly_non_executable(self):
        calls = self.format('{"name":"bash"}')
        self.assertEqual(calls[0]['function'], {'name':'bash', 'arguments':'null'})

    def test_invalid_types_and_truncated_output_are_not_server_exceptions(self):
        for text in ['{', 'null', '[]', '"text"', '{"name":3,"arguments":{}}', '{"name":"read","arguments":[]}']:
            with self.subTest(text=text):
                call = self.format(text)[0]
                self.assertEqual(call['function']['arguments'], 'null')

    def test_valid_call_and_failed_call_keep_distinct_stream_indices(self):
        calls = self.format('[{"name":"read","arguments":{"path":"a.txt"}},{"name":"bash"}]', True)
        self.assertEqual([c['index'] for c in calls], [0, 1])
        self.assertEqual(json.loads(calls[0]['function']['arguments']), {'path':'a.txt'})
        self.assertEqual(calls[1]['function']['arguments'], 'null')
        self.assertNotEqual(calls[0]['id'], calls[1]['id'])

    def test_formatting_does_not_mutate_parser_owned_objects(self):
        parsed = {'id':'old', 'name':'read', 'arguments':{'path':'a.txt'}}
        formatter = ToolCallFormatter(lambda text, tools: parsed, [])
        formatter(['x'])
        self.assertEqual(parsed, {'id':'old', 'name':'read', 'arguments':{'path':'a.txt'}})

    def test_backend_failures_are_not_mislabelled_as_model_errors(self):
        def fail(text, tools):
            raise MemoryError('backend allocation failed')
        with self.assertRaises(MemoryError):
            ToolCallFormatter(fail, [])(['anything'])


if __name__ == '__main__':
    unittest.main()
