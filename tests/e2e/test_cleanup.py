import json
import unittest
from unittest.mock import patch

import pytest

import conftest
import test_deployed


class Clock:
    now = 0

    def sleep(self, seconds):
        self.now += seconds


class Api:
    def __init__(self, states, listed=None, delete_codes=None):
        self.states = {key: list(values) for key, values in states.items()}
        self.listed = listed if listed is not None else list(states)
        self.delete_codes = delete_codes or {}
        self.calls = []

    def json(self, method, path, data=None, **kwargs):
        if method == 'POST':
            return {'id': 'agent'}
        if isinstance(self.listed, Exception):
            raise self.listed
        return {'data': [{'id': session} for session in self.listed]}

    def request(self, method, path, **kwargs):
        self.calls.append((method, path))
        session = path.rsplit('/', 1)[-1]
        if method == 'DELETE':
            value = self.delete_codes.get(session, 200)
            if isinstance(value, Exception):
                raise value
            return value, {}, b'{}'
        states = self.states[session]
        value = states.pop(0) if len(states) > 1 else states[0]
        return 200, {}, json.dumps(value).encode()


class CleanupTests(unittest.TestCase):
    def setUp(self):
        self.clock = Clock()
        self.monotonic = patch.object(conftest.time, 'monotonic', lambda: self.clock.now)
        self.sleep = patch.object(conftest.time, 'sleep', self.clock.sleep)
        self.monotonic.start()
        self.sleep.start()
        self.addCleanup(self.monotonic.stop)
        self.addCleanup(self.sleep.stop)

    def test_running_and_finalizing_wait_without_close_or_cancel(self):
        api = Api({'session': [{'status': 'running', 'run_id': 'run'},
                               {'status': 'idle', 'run_id': 'run'},
                               {'status': 'idle', 'run_id': None}]})
        self.assertEqual(conftest.cleanup_deployed_agent(api, 'agent', ['session'], timeout=8), [])
        self.assertEqual(api.calls[:3], [('GET', '/v1/sessions/session')] * 3)
        self.assertEqual(api.calls[-2:], [('DELETE', '/v1/sessions/session'), ('DELETE', '/v1/agents/agent')])
        self.assertEqual(self.clock.now, 4)

    def test_timeout_does_not_abandon_other_sessions_or_agent(self):
        api = Api({'busy': [{'status': 'running', 'run_id': 'run'}],
                   'other': [{'status': 'idle', 'run_id': None}]})
        failures = conftest.cleanup_deployed_agent(api, 'agent', ['busy', 'other'], timeout=4)
        self.assertTrue(failures[0]['deadline_exceeded'])
        self.assertNotIn(('DELETE', '/v1/sessions/busy'), api.calls)
        self.assertIn(('DELETE', '/v1/sessions/other'), api.calls)
        self.assertEqual(api.calls[-1], ('DELETE', '/v1/agents/agent'))
        self.assertEqual(self.clock.now, 4)

    def test_cleanup_errors_are_aggregated_without_private_messages(self):
        api = Api({'one': [{'status': 'idle'}], 'two': [{'status': 'closed'}]},
                  listed=RuntimeError('PRIVATE_DATA'),
                  delete_codes={'one': RuntimeError('PRIVATE_DATA'), 'agent': 409})
        failures = conftest.cleanup_deployed_agent(api, 'agent', ['one', 'two'], timeout=4)
        self.assertEqual(len(failures), 3)
        self.assertIn(('DELETE', '/v1/sessions/two'), api.calls)
        self.assertNotIn('PRIVATE_DATA', json.dumps(failures))

    def test_discovers_implicit_session_after_sdk_timeout(self):
        api = Api({'untracked': [{'status': 'idle'}]})
        self.assertEqual(conftest.cleanup_deployed_agent(api, 'agent', [], timeout=4), [])
        self.assertIn(('DELETE', '/v1/sessions/untracked'), api.calls)

    def test_original_failure_remains_a_failure_after_successful_cleanup(self):
        api = Api({})
        fixture = test_deployed.managed_agent.__wrapped__(api)
        next(fixture)
        with self.assertRaisesRegex(AssertionError, 'original failure'):
            fixture.throw(AssertionError('original failure'))
        self.assertIn(('DELETE', '/v1/agents/agent'), api.calls)
        failed_api = Api({}, delete_codes={'agent': 409})
        fixture = test_deployed.managed_agent.__wrapped__(failed_api)
        next(fixture)
        original = AssertionError('original failure')
        with pytest.raises(pytest.fail.Exception) as cleanup_failure:
            fixture.throw(original)
        self.assertIs(cleanup_failure.value.__context__, original)


if __name__ == '__main__':
    unittest.main(verbosity=2)
