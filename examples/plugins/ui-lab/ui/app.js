(function () {
  const counterEl = document.getElementById("counter");
  const toolEl = document.getElementById("tool");
  const sessionEl = document.getElementById("session");

  function render(state) {
    if (!state) return;
    if (state.counter !== undefined) counterEl.textContent = String(state.counter);
    if (state.lastTool !== undefined) toolEl.textContent = String(state.lastTool);
  }

  window.kroma.onEvent((ev) => {
    if (!ev) return;
    if (ev.kind === "context") {
      sessionEl.textContent = ev.session_id || "default";
    } else if (ev.kind === "state") {
      render(ev);
    }
  });

  document.getElementById("btn-inc").addEventListener("click", async () => {
    const next = await window.kroma.request({ action: "increment" });
    render(next);
  });

  document.getElementById("btn-composer").addEventListener("click", () => {
    window.kroma.setComposer(`UI Lab 当前计数：${counterEl.textContent}`);
  });

  document.getElementById("btn-notify").addEventListener("click", () => {
    window.kroma.notify("来自自定义桌面 HTML 面板的问候！");
  });

  window.kroma.request({ action: "state" }).then(render).catch(() => {});
})();
