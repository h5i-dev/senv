"""Improving Language Model Negotiation with Self-Play and In-Context
Learning from AI Feedback (Fu et al. 2023, arXiv:2305.10142) — bargaining
games where the coach's notes are the learning signal.

Buyer and seller bargain over an item; after each game a critic reviews
the coached side's play and writes concrete strategy feedback; the next
game starts with all previous games and feedback in context. No weights
move — improvement (a better deal price, game over game) comes purely
from in-context learning on AI feedback. Moves are validated JSON, the
price trajectory is host-side state, and every game is journaled.

    python examples/papers/negotiation.py ["<item description>"]
"""

import asyncio
import sys
from collections.abc import Mapping
from typing import Any

from h5i.orchestra import Conductor

DEMO_ITEM = "a used mountain bike in good condition, listed at $200"
N_GAMES = 3
MAX_TURNS = 6  # moves per game (buyer and seller alternating)


def parse_move(value: Any) -> dict[str, Any]:
    if not isinstance(value, Mapping) or "message" not in value:
        raise ValueError(
            'reply must be {"message": "...", "offer": <number or null>, '
            '"accept": true|false}'
        )
    offer = value.get("offer")
    return {
        "message": str(value["message"]).strip(),
        "offer": float(offer) if offer is not None else None,
        "accept": bool(value.get("accept", False)),
    }


def parse_text(value: Any) -> str:
    return value if isinstance(value, str) else str(value)


async def main(item: str) -> None:
    async with Conductor(".", "negotiation-demo", launcher="resident", isolation="supervised") as c:
        buyer = await c.hire("buyer", runtime="claude", model="claude-haiku-4-5")
        seller = await c.hire("seller", runtime="codex", model="gpt-5.4-mini", effort="medium")
        critic = await c.hire("critic", runtime="claude", model="claude-haiku-4-5")

        lessons = ""  # the buyer's accumulated AI feedback
        prices: list[float | None] = []
        for game in range(1, N_GAMES + 1):
            transcript = ""
            deal: float | None = None
            last_offer: float | None = None
            for turn in range(MAX_TURNS):
                mover, role_brief = (
                    (buyer, "You are the BUYER: your goal is the LOWEST price.")
                    if turn % 2 == 0
                    else (seller, "You are the SELLER: your goal is the HIGHEST price.")
                )
                coaching = (
                    f"\nYour coach's notes from earlier games:\n{lessons}\n"
                    if lessons and mover is buyer
                    else ""
                )
                move = await mover.ask(
                    f"Negotiation over: {item}\n{role_brief}{coaching}\n"
                    f"Dialogue so far:\n{transcript or '(you open)'}\n\n"
                    "Make your next move: a short message, your current "
                    "offer (number, or null if none yet), and accept=true "
                    "ONLY if you take the opponent's last offer. Reply as "
                    'JSON: {"message": "...", "offer": <number|null>, '
                    '"accept": true|false}',
                    parse=parse_move,
                )
                who = "buyer" if mover is buyer else "seller"
                transcript += f"{who}: {move['message']} (offer: {move['offer']})\n"
                if move["accept"] and last_offer is not None:
                    deal = last_offer
                    break
                if move["offer"] is not None:
                    last_offer = move["offer"]
            prices.append(deal)
            print(f"game {game}: " + (f"deal at ${deal:.0f}" if deal else "no deal"))

            if game == N_GAMES:
                break

            # AI feedback: the critic coaches the buyer for the next game.
            feedback = await critic.ask(
                f"You coach the BUYER in negotiations over: {item}\n\n"
                f"Game {game} transcript:\n{transcript}\n"
                + (f"Deal price: ${deal:.0f}\n" if deal else "No deal was reached.\n")
                + "Give 2-3 concrete strategy improvements for the buyer's "
                "next game. Reply as a single JSON string.",
                parse=parse_text,
            )
            lessons += f"After game {game}: {feedback}\n"

        trajectory = ", ".join(f"${p:.0f}" if p else "no deal" for p in prices)
        await c.note(f"negotiation self-play: deal trajectory {trajectory}")
        print(f"\ndeal price trajectory (buyer coached): {trajectory}")


if __name__ == "__main__":
    asyncio.run(main(sys.argv[1] if len(sys.argv) > 1 else DEMO_ITEM))
