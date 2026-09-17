import io
import json
import unittest
from unittest.mock import patch

import local_repair_adapter as adapter


class AdapterTests(unittest.TestCase):
    context = {"selector": "#billing .old", "html": '<div id="billing"><a class="new"></a></div>', "bounds": [0, 0, 100, 40]}

    def generate(self, envelope):
        return patch("urllib.request.OpenerDirector.open", return_value=io.BytesIO(json.dumps(envelope).encode()))

    def test_local_request_and_candidate(self):
        with self.generate({"done": True, "response": '{"selector":"#billing .new"}'}) as send:
            self.assertEqual(adapter.repair(self.context, "local-model"), {"selector": "#billing .new"})
            request = send.call_args.args[0]
            self.assertEqual(request.full_url, adapter.ENDPOINT)
            payload = json.loads(request.data)
            self.assertFalse(payload["stream"])
            self.assertEqual(json.loads(payload["prompt"]), self.context)
            self.assertEqual(send.call_args.kwargs["timeout"], 25)

    def test_rejects_invalid_model_outputs(self):
        for response in ('{"selector":""}', '{"selector":7}', '{"selector":"a","extra":1}', '```json\n{"selector":"a"}\n```', '{"selector":' + json.dumps("a" * 2049) + '}'):
            with self.subTest(response=response[:30]), self.generate({"done": True, "response": response}):
                with self.assertRaises(ValueError):
                    adapter.repair(self.context, "local-model")

    def test_rejects_incomplete_response(self):
        with self.generate({"done": False, "response": '{"selector":"a"}'}):
            with self.assertRaises(ValueError):
                adapter.repair(self.context, "local-model")

    def test_invalid_input_never_contacts_model(self):
        with patch("urllib.request.OpenerDirector.open") as send:
            for context, model in ((self.context, ""), ({}, "local"), ({**self.context, "bounds": [True, 0, 1, 1]}, "local")):
                with self.assertRaises(ValueError):
                    adapter.repair(context, model)
            send.assert_not_called()

    def test_redirect_rejected(self):
        with self.assertRaises(ValueError):
            adapter.NoRedirect().redirect_request(None, None, 302, "", {}, "https://example.com")


if __name__ == "__main__":
    unittest.main()
