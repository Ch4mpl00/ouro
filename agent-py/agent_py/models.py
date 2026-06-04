"""Named model presets.

Each preset binds a concrete model to its ``reasoning_effort``. Sessions pick a
preset by name rather than tuning model + effort separately: in practice the
two are coupled (a cheap non-thinking chat model, an expensive thinking model
at max effort), and the pair name reads as intent ("base reply", "smart
digest").
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Literal

from langchain.chat_models import init_chat_model
from langchain_core.language_models import BaseChatModel

ReasoningEffort = Literal["disabled", "low", "medium", "high", "xhigh"]
PresetName = Literal["base", "smart", "smartest"]


@dataclass(frozen=True)
class ModelPreset:
    # `provider:model`, e.g. "openai:gpt-5.4-mini" or "deepseek:deepseek-v4-pro".
    # init_chat_model reads the provider prefix and picks the matching env API
    # key (OPENAI_API_KEY / DEEPSEEK_API_KEY).
    model: str
    reasoning_effort: ReasoningEffort


# Defaults applied at engine start; the supervisor layers env overrides on top
# (see supervisor/main.py).
DEFAULT_PRESETS: dict[str, ModelPreset] = {
    # base     — non-thinking chat on OpenAI. The default for ordinary replies.
    "base": ModelPreset(model="openai:gpt-5.4-mini", reasoning_effort="disabled"),
    # smart    — DeepSeek with thinking enabled. Sub-agents doing real
    #            editorial / parsing work.
    "smart": ModelPreset(model="deepseek:deepseek-v4-pro", reasoning_effort="high"),
    # smartest — full GPT-5.4. Reserved for the compiler role, where strict
    #            structured output matters more than cost per call.
    "smartest": ModelPreset(model="openai:gpt-5.4", reasoning_effort="high"),
}

PRESET_NAMES: tuple[PresetName, ...] = ("base", "smart", "smartest")


def build_model(preset: ModelPreset) -> BaseChatModel:
    """Construct a LangChain chat model from a preset.

    ``init_chat_model`` selects the provider class (ChatOpenAI / ChatDeepSeek)
    from the ``provider:model`` string and reads the matching env API key.
    ``reasoning_effort`` is forwarded to the underlying class (None for the
    non-thinking ``base`` preset).
    """
    effort = None if preset.reasoning_effort == "disabled" else preset.reasoning_effort
    return init_chat_model(preset.model, reasoning_effort=effort)
