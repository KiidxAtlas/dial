// Shows whether this Chrome is connected to the Dial relay, and offers a reconnect.
const statusEl = document.getElementById("status");
const detailEl = document.getElementById("detail");

function render(state) {
  if (!state) {
    statusEl.textContent = "Not running";
    detailEl.textContent = "The extension service worker did not answer.";
    return;
  }
  if (state.connected) {
    statusEl.textContent = "Connected to Dial";
    detailEl.textContent = `Relay on 127.0.0.1:${state.port}.`;
    return;
  }
  statusEl.textContent = "Not connected";
  detailEl.textContent =
    (state.lastError ? `${state.lastError}. ` : "") +
    "Start Dial with browser relay enabled, or run `dial browser-relay status` to check.";
}

function refresh() {
  chrome.runtime.sendMessage({ t: "status" }, (state) => {
    void chrome.runtime.lastError;
    render(state);
  });
}

document.getElementById("reconnect").addEventListener("click", () => {
  chrome.runtime.sendMessage({ t: "reconnect" }, () => {
    void chrome.runtime.lastError;
    setTimeout(refresh, 500);
  });
});

refresh();
