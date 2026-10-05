"""Dial-owned local typed-decision service. No coding-agent framework dependency."""
from contextlib import asynccontextmanager
import logging
import os
from pathlib import Path
import signal
import threading
import time

from fastapi import FastAPI, HTTPException, Request
from pydantic import BaseModel, Field

log = logging.getLogger("dial.laya")
logging.basicConfig(level=logging.INFO)
HOME = Path(os.environ["DIAL_LAYA_HOME"])
CHECKPOINT = os.environ.get("DIAL_LAYA_CHECKPOINT") or str(HOME / "model")
IDLE_SECONDS = float(os.environ.get("DIAL_LAYA_IDLE_TIMEOUT_SECONDS", "900"))
agent = None
device = "loading"
last_activity = time.monotonic()
prediction_lock = threading.Lock()


def load_agent():
    global agent, device
    try:
        import laya
        import torch
        selected = "mps" if torch.backends.mps.is_available() else "cpu"
        loaded = laya.load(CHECKPOINT, device=selected)
        loaded.cfg["max_len"] = 4096
        agent, device = loaded, selected
        log.info("Dial Laya ready: %s on %s", CHECKPOINT, selected)
    except Exception:
        log.exception("Dial Laya checkpoint could not load")
        os.kill(os.getpid(), signal.SIGTERM)


def watch_idle(stop):
    while not stop.wait(10):
        if IDLE_SECONDS > 0 and time.monotonic() - last_activity > IDLE_SECONDS:
            log.info("Dial Laya idle shutdown")
            os.kill(os.getpid(), signal.SIGTERM)
            return


@asynccontextmanager
async def lifespan(_):
    # Reserve the listening port before loading weights; concurrent launches cannot load twice.
    stop = threading.Event()
    threading.Thread(target=load_agent, daemon=True).start()
    threading.Thread(target=watch_idle, args=(stop,), daemon=True).start()
    try:
        yield
    finally:
        stop.set()


app = FastAPI(lifespan=lifespan)


@app.middleware("http")
async def activity(request: Request, call_next):
    global last_activity
    if request.url.path != "/health":
        last_activity = time.monotonic()
    return await call_next(request)


@app.get("/health")
def health():
    return {"service": "dial-laya", "protocol": 1,
            "status": "ok" if agent is not None else "starting",
            "checkpoint": CHECKPOINT, "device": device}


class Prediction(BaseModel):
    state: dict[str, str] = Field(max_length=4)
    questions: dict[str, dict] = Field(min_length=1, max_length=16)


@app.post("/v1/predict")
def predict(request: Prediction):
    if agent is None:
        raise HTTPException(503, "Dial Laya is loading")
    if sum(len(text) for text in request.state.values()) > 12000:
        raise HTTPException(422, "Evidence packet exceeds 12000 characters")
    for question in request.questions.values():
        if question.get("type") not in ("choice", "noul", "score"):
            raise HTTPException(422, "Unsupported typed question")
    try:
        with prediction_lock:
            return agent.predict(request.state, request.questions)
    except Exception:
        log.exception("Dial Laya prediction failed")
        raise HTTPException(500, "Dial Laya prediction failed")
