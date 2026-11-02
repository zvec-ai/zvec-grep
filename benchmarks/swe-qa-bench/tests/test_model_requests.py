"""Execution and judging must use the same declared model requests."""

from __future__ import annotations

import unittest
from unittest.mock import patch

from zg_bench.core.protocol import JUDGE_MODELS, model_request_spec
from zg_bench.engines.judge import judge_generation_metadata, judge_temperature
from zg_bench.engines.registry import resolve_agent_model, resolve_opencode_model


class ModelRequestTests(unittest.TestCase):
    def test_qwen_versions_have_identical_execution_parameters(self) -> None:
        from dataclasses import replace

        old = resolve_agent_model("opencode", "custom-openai/qwen3.7-max").opencode
        new = resolve_agent_model("opencode", "custom-openai/qwen3.8-max").opencode
        self.assertEqual(
            replace(old, model_id=new.model_id, display_name=new.display_name), new
        )

    def test_custom_execution_and_judging_share_parameters(self) -> None:
        for model in JUDGE_MODELS:
            with self.subTest(model=model):
                execution = resolve_agent_model("opencode", f"custom-openai/{model}")
                provider = execution.opencode
                self.assertIsNotNone(provider)
                assert provider is not None
                request = model_request_spec(model)
                self.assertEqual(provider.temperature, request.temperature)
                self.assertEqual(provider.enable_thinking, request.enable_thinking)
                self.assertEqual(provider.reasoning_effort, request.reasoning_effort)
                self.assertEqual(provider.output_limit, request.max_tokens)
                self.assertEqual(judge_temperature(model), request.temperature)
                self.assertEqual(
                    judge_generation_metadata(model),
                    {
                        "enable_thinking": request.enable_thinking,
                        "reasoning_effort": request.reasoning_effort,
                        "max_tokens": request.max_tokens,
                        "response_format": None,
                    },
                )

    def test_execution_and_judge_models_share_endpoint_and_key_priority(self) -> None:
        with patch.dict(
            "os.environ",
            {
                "GLM_API_KEY": "preferred-key",
                "OPENAI_API_KEY": "fallback-key",
                "GLM_BASE_URL": "https://preferred.invalid/v1",
                "OPENAI_BASE_URL": "https://fallback.invalid/v1",
            },
            clear=True,
        ):
            for model in JUDGE_MODELS:
                with self.subTest(model=model):
                    execution = resolve_opencode_model(
                        f"custom-openai/{model}", require_credentials=True
                    )
                    # The judge explicitly selects the comparison endpoint;
                    # bare qwen3.7-max remains a legacy DashScope alias.
                    judge = resolve_opencode_model(
                        f"custom-openai/{model}", require_credentials=True
                    )
                    self.assertEqual(execution, judge)
                    self.assertEqual(judge.credential_name, "GLM_API_KEY")
                    self.assertEqual(judge.api_key, "preferred-key")
                    self.assertEqual(
                        judge.configuration.base_url,
                        "https://preferred.invalid/v1",
                    )
