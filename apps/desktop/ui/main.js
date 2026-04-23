const invoke = window.__TAURI__?.core?.invoke;

const state = {
  selectedDeviceId: null,
  selectedAgentHost: null,
  refreshTimer: null,
  busy: false,
};

async function loadDashboard() {
  bindStaticActions();
  if (!invoke) {
    ensureInvokeReady({ log: true });
  }
  await refreshDashboard({ includeLogs: true, silent: false });
  state.refreshTimer = setInterval(() => refreshDashboard({ includeLogs: false, silent: true }), 2500);
}

async function fetchDashboard() {
  if (invoke) {
    const [dashboard, adbTransfers] = await Promise.all([
      invoke("bootstrap_dashboard"),
      invoke("list_adb_transfers"),
    ]);
    return { dashboard, adbTransfers };
  }

  return {
    dashboard: {
      app_name: "Nekotrans",
      transport_modes: ["ADB-only", "Wi-Fi-only", "Dual Track"],
      devices: [],
      tasks: [],
      recoverable_tasks: [],
      sample_logs: [],
    },
    adbTransfers: [],
  };
}

async function refreshDashboard({ includeLogs = false, silent = true } = {}) {
  if (state.busy && silent) {
    return;
  }

  try {
    const { dashboard, adbTransfers } = await fetchDashboard();
    const allTasks = dashboard.tasks || [];
    const visibleTasks = visibleTaskCards(allTasks);
    autoSelectDevice(dashboard.devices || []);
    renderConnectionOverview(dashboard);
    renderDevices(dashboard.devices || []);
    renderTasks(visibleTasks, adbTransfers || []);
    renderRecoverables(dashboard.recoverable_tasks || [], allTasks);
    renderAdbTransfers(adbTransfers || [], allTasks);
    if (includeLogs) {
      await refreshLogs();
    }
    setRefreshState("实时");
  } catch (error) {
    setRefreshState("离线");
    showToast(String(error), true);
  }
}

function visibleTaskCards(tasks) {
  return tasks
    .filter((task) => task.task_id !== "demo-task")
    .sort((left, right) => {
      const rank = { Running: 0, Failed: 1, Paused: 2, Pending: 3, Completed: 4, Cancelled: 5 };
      return (rank[left.state] ?? 10) - (rank[right.state] ?? 10);
    });
}

function autoSelectDevice(devices) {
  if (state.selectedDeviceId || !devices.length) {
    updateDeviceStep(devices);
    return;
  }
  const best = devices.find((device) => device.adb_ready && device.agent_host) ||
    devices.find((device) => device.adb_ready) ||
    devices[0];
  if (best) {
    fillDraftFromDevice(best.id, best.agent_host || "", best.adb_ready && best.agent_host ? "Dual Track" : best.adb_ready ? "ADB-only" : "Wi-Fi-only", {
      silent: true,
    });
  }
  updateDeviceStep(devices);
}

function updateDeviceStep(devices) {
  const ready = devices.find((device) => device.adb_ready && device.agent_host);
  const adbOnly = devices.find((device) => device.adb_ready);
  const message = ready
    ? `已选择 ${ready.label}，双通道可用：${ready.id} + ${ready.agent_host}:38997`
    : adbOnly
      ? `已检测到 ${adbOnly.label}，ADB 可用；等待手机端 Wi-Fi 代理地址。`
      : "请连接手机并允许 USB 调试。";
  text("#device-step-text", message);
}

function renderConnectionOverview(dashboard) {
  const devices = dashboard.devices || [];
  const tasks = (dashboard.tasks || []).filter((task) => task.task_id !== "demo-task");
  const dualReady = devices.filter((device) => device.adb_ready && device.agent_host).length;
  const activeTasks = tasks.filter((task) => ["Pending", "Running", "Paused", "Failed"].includes(task.state)).length;

  text("#metric-devices", devices.length);
  text("#metric-dual-ready", dualReady);
  text("#metric-active-tasks", activeTasks);

  const overview = document.querySelector("#link-overview");
  const best = devices.find((device) => device.id === state.selectedDeviceId) ||
    devices.find((device) => device.adb_ready && device.agent_host) ||
    devices[0];

  if (!best) {
    overview.innerHTML = `<div class="empty-state">没有已连接的 Android 设备。</div>`;
    return;
  }

  overview.innerHTML = `
    <div class="endpoint-node">
      <span>Windows</span>
      <strong>调度器</strong>
    </div>
    <div class="route-lines">
      <span class="${best.adb_ready ? "good" : "warn"}">ADB</span>
      <span class="${best.agent_host ? "good" : "warn"}">Wi-Fi</span>
    </div>
    <div class="endpoint-node">
      <span>Android</span>
      <strong>${escapeHtml(best.label)}</strong>
      <small>${escapeHtml(best.agent_host ? `${best.agent_host}:38997` : best.id)}</small>
    </div>
  `;
}

function renderDevices(devices) {
  const host = document.querySelector("#device-list");
  if (!devices.length) {
    host.innerHTML = `<div class="empty-state">请连接已开启 USB 调试的手机。</div>`;
    return;
  }

  host.innerHTML = devices.map((device) => {
    const selected = device.id === state.selectedDeviceId ? " selected" : "";
    const agentHost = device.agent_host || "";
    return `
      <div class="device-card${selected}" data-device-card="${escapeHtml(device.id)}">
        <div class="device-main">
          <div>
            <strong>${escapeHtml(device.label)}</strong>
            <p>${escapeHtml(device.id)}</p>
          </div>
          <div class="badge-row">
            <span class="badge ${device.adb_ready ? "good" : "warn"}">ADB</span>
          <span class="badge ${agentHost ? "good" : "warn"}">Wi-Fi</span>
          </div>
        </div>
        <div class="device-paths">
          <span>${escapeHtml(device.platform_text)}</span>
          <strong>${escapeHtml(agentHost ? `${agentHost}:38997` : "未发现代理地址")}</strong>
        </div>
        <div class="action-row">
          <button class="secondary-btn" data-use-mode="Dual Track" data-device="${escapeHtml(device.id)}" data-host="${escapeHtml(agentHost)}" ${device.adb_ready && agentHost ? "" : "disabled"}>双通道</button>
          <button class="secondary-btn" data-use-mode="ADB-only" data-device="${escapeHtml(device.id)}" data-host="${escapeHtml(agentHost)}" ${device.adb_ready ? "" : "disabled"}>ADB</button>
          <button class="secondary-btn" data-use-mode="Wi-Fi-only" data-device="${escapeHtml(device.id)}" data-host="${escapeHtml(agentHost)}" ${agentHost ? "" : "disabled"}>Wi-Fi</button>
          <button class="icon-btn" title="安装手机端代理" data-install="${escapeHtml(device.id)}" ${device.adb_ready ? "" : "disabled"}>↓</button>
        </div>
      </div>
    `;
  }).join("");

  for (const button of host.querySelectorAll("[data-use-mode]")) {
    button.addEventListener("click", (event) => {
      const target = event.currentTarget;
      fillDraftFromDevice(target.dataset.device, target.dataset.host, target.dataset.useMode);
    });
  }

  for (const button of host.querySelectorAll("[data-install]")) {
    button.addEventListener("click", async (event) => {
      await withBusy(async () => {
        const serial = event.currentTarget.dataset.install;
        await invoke("install_agent", { serial });
        showToast("手机端代理已安装。");
      });
      await refreshDashboard({ includeLogs: false, silent: false });
    });
  }
}

function fillDraftFromDevice(serial, agentHost, mode, options = {}) {
  state.selectedDeviceId = serial || null;
  state.selectedAgentHost = agentHost || null;
  setValue("#draft-device-serial", serial || "");
  setValue("#draft-agent-host", agentHost || "");
  setValue("#agent-host-input", agentHost || "");
  setValue("#draft-transport-mode", mode || "Dual Track");
  setSegment(".segment[data-mode]", mode || "Dual Track", "mode");
  setValue("#draft-direction", "PC -> Android");
  setSegment(".segment[data-direction]", "PC -> Android", "direction");
  if (!value("#draft-target-root")) {
    setValue("#draft-target-root", "/sdcard/Nekotrans");
  }
  document.querySelector("#draft-verify").checked = true;
  text("#selected-device-badge", serial ? `${serial}${agentHost ? ` / ${agentHost}` : ""}` : "未选择设备");
  syncDraftDirectionUi();
  if (!options.silent) {
    showToast(mode === "Dual Track" ? "已选择双通道设备。" : "已选择设备。");
  }
}

function renderTasks(tasks, transfers = []) {
  const host = document.querySelector("#task-list");
  if (!tasks.length) {
    host.innerHTML = `<div class="empty-state">暂无传输任务。选择来源后点击“创建并启动”。</div>`;
    return;
  }
  const transferByTask = new Map(transfers.map((transfer) => [transfer.task_id, transfer]));

  host.innerHTML = tasks.map((task) => {
    const transfer = transferByTask.get(task.task_id);
    const canStart = task.state === "Pending";
    const canPause = task.state === "Running";
    const canResume = task.state === "Paused";
    const canRetry = task.state === "Failed";
    const canCancel = !["Completed", "Cancelled"].includes(task.state);
    const stateClass = task.state === "Completed" ? "good" : task.state === "Failed" ? "bad" : task.state === "Paused" ? "accent" : "";
    return `
      <div class="task-card">
        <div class="task-header">
          <div>
            <strong>${escapeHtml(task.task_id)}</strong>
          <p>${escapeHtml(displayDirection(task.direction))} / ${escapeHtml(displayMode(task.transport_mode))}</p>
          </div>
          <span class="badge ${stateClass}">${escapeHtml(displayState(task.state))}</span>
        </div>
        <div class="progress-bar"><div class="progress-fill" style="width:${task.progress_percent}%"></div></div>
        <div class="task-stats">
          <span>${task.progress_percent}%</span>
          <span>${formatBytes(task.committed_bytes)} / ${formatBytes(task.total_bytes)}</span>
          <span>ADB ${formatBytes(task.adb_bytes)}</span>
          <span>Wi-Fi ${formatBytes(task.wifi_bytes)}</span>
        </div>
        ${transfer ? `<p class="stage-line">${escapeHtml(displayWorkerStage(transfer.last_event, transfer.last_message, task))}</p>` : ""}
        ${task.last_error ? `<p class="error-line">${escapeHtml(task.last_error)}</p>` : ""}
        <div class="action-row">
          <button class="secondary-btn" data-start-task="${escapeHtml(task.task_id)}" ${canStart ? "" : "disabled"}>启动</button>
          <button class="secondary-btn" data-pause-task="${escapeHtml(task.task_id)}" ${canPause ? "" : "disabled"}>暂停</button>
          <button class="secondary-btn" data-resume-task="${escapeHtml(task.task_id)}" ${task.state === "Paused" ? "" : "disabled"}>继续</button>
          <button class="secondary-btn" data-retry-task="${escapeHtml(task.task_id)}" ${canRetry ? "" : "disabled"}>重试</button>
          <button class="secondary-btn" data-delete-task="${escapeHtml(task.task_id)}" ${task.state === "Running" ? "disabled" : ""}>删除记录</button>
          <button class="danger-btn" data-cancel-task="${escapeHtml(task.task_id)}" ${canCancel ? "" : "disabled"}>取消</button>
        </div>
      </div>
    `;
  }).join("");

  bindTaskButtons(host);
}

function renderAdbTransfers(transfers, tasks = []) {
  const host = document.querySelector("#adb-transfer-list");
  const taskIds = new Set(tasks.map((task) => task.task_id));
  const visibleTransfers = transfers.filter((transfer) => !taskIds.has(transfer.task_id));
  if (!visibleTransfers.length) {
    host.innerHTML = "";
    return;
  }

  host.innerHTML = visibleTransfers.map((transfer) => {
    const totalFiles = transfer.total_files || 1;
    const completedFiles = transfer.pushed_files + transfer.skipped_files;
    const progress = Math.min(100, Math.round((completedFiles * 100) / totalFiles));
    return `
      <div class="worker-card">
        <div class="task-header">
          <div>
            <strong>${escapeHtml(transfer.task_id)}</strong>
            <p>${escapeHtml(transfer.serial)} / ${escapeHtml(transfer.relative_path || "preparing")}</p>
          </div>
          <span class="badge">${escapeHtml(transfer.last_event || transfer.state)}</span>
        </div>
        <div class="progress-bar"><div class="progress-fill" style="width:${progress}%"></div></div>
        <div class="task-stats">
          <span>文件 ${completedFiles}/${transfer.total_files}</span>
          <span>${formatBytes(transfer.bytes_pushed)}</span>
          <span>${escapeHtml(transfer.last_message || "")}</span>
        </div>
      </div>
    `;
  }).join("");
}

function renderRecoverables(taskIds, tasks = []) {
  const host = document.querySelector("#recoverable-list");
  const currentTaskIds = new Set(tasks.map((task) => task.task_id));
  const visibleTaskIds = taskIds.filter((taskId) => taskId !== "demo-task" && !currentTaskIds.has(taskId));
  const loadedRecoverables = tasks.filter((task) =>
    task.task_id !== "demo-task" && ["Paused", "Failed"].includes(task.state)
  );
  if (!visibleTaskIds.length && !loadedRecoverables.length) {
    host.innerHTML = `<div class="empty-state">没有可恢复的检查点。</div>`;
    return;
  }

  const loadedRows = loadedRecoverables.map((task) => `
    <div class="recoverable-row">
      <div>
        <strong>${escapeHtml(task.task_id)}</strong>
        <p>${escapeHtml(displayState(task.state))}，已加载到传输列表</p>
      </div>
      <div class="action-row">
        <button class="secondary-btn" data-resume-task="${escapeHtml(task.task_id)}" ${task.state === "Paused" ? "" : "disabled"}>继续</button>
        <button class="secondary-btn" data-retry-task="${escapeHtml(task.task_id)}" ${task.state === "Failed" ? "" : "disabled"}>重试</button>
        <button class="danger-btn" data-delete-task="${escapeHtml(task.task_id)}">删除记录</button>
      </div>
    </div>
  `).join("");

  const orphanRows = visibleTaskIds.map((taskId) => `
    <div class="recoverable-row">
      <strong>${escapeHtml(taskId)}</strong>
      <div class="action-row">
        <button class="secondary-btn" data-recover-task="${escapeHtml(taskId)}">恢复</button>
        <button class="danger-btn" data-delete-task="${escapeHtml(taskId)}">删除记录</button>
      </div>
    </div>
  `).join("");
  host.innerHTML = loadedRows + orphanRows;

  for (const button of host.querySelectorAll("[data-recover-task]")) {
    button.addEventListener("click", async (event) => {
      await mutateTask("recover_task", { taskId: event.currentTarget.dataset.recoverTask });
    });
  }
  for (const button of host.querySelectorAll("[data-delete-task]")) {
    button.addEventListener("click", async (event) => {
      await deleteTaskRecord(event.currentTarget.dataset.deleteTask);
    });
  }
  bindTaskButtons(host);
}

function bindTaskButtons(host) {
  const actions = [
    ["start_transfer_task", "startTask"],
    ["pause_transfer_task", "pauseTask"],
    ["resume_transfer_task", "resumeTask"],
    ["retry_transfer_task", "retryTask"],
    ["cancel_transfer_task", "cancelTask"],
    ["delete_transfer_task", "deleteTask"],
  ];
  for (const [command, key] of actions) {
    for (const button of host.querySelectorAll(`[data-${kebab(key)}]`)) {
      button.addEventListener("click", async (event) => {
        if (command === "delete_transfer_task") {
          await deleteTaskRecord(event.currentTarget.dataset[key]);
        } else {
          await mutateTask(command, { taskId: event.currentTarget.dataset[key] });
        }
      });
    }
  }
}

function bindStaticActions() {
  document.querySelector("#refresh-devices-btn")?.addEventListener("click", () => refreshDashboard({ includeLogs: false, silent: false }));
  document.querySelector("#refresh-recover-btn")?.addEventListener("click", () => refreshDashboard({ includeLogs: false, silent: false }));
  document.querySelector("#refresh-logs-btn")?.addEventListener("click", () => refreshLogs());
  document.querySelector("#export-logs-btn")?.addEventListener("click", () => exportLogs());
  document.querySelector("#fetch-agent-logs-btn")?.addEventListener("click", () => fetchAgentLogs());
  document.querySelector("#probe-agent-btn")?.addEventListener("click", () => probeAgentHost());
  document.querySelector("#pick-source-file-btn")?.addEventListener("click", () => pickDraftSourcePath(false));
  document.querySelector("#pick-source-folder-btn")?.addEventListener("click", () => pickDraftSourcePath(true));
  document.querySelector("#pick-target-folder-btn")?.addEventListener("click", () => pickDraftTargetFolder());
  document.querySelector("#draft-form")?.addEventListener("submit", stageDraft);

  for (const button of document.querySelectorAll(".segment[data-direction]")) {
    button.addEventListener("click", () => {
      setValue("#draft-direction", button.dataset.direction);
      setSegment(".segment[data-direction]", button.dataset.direction, "direction");
      syncDraftDirectionUi();
    });
  }
  for (const button of document.querySelectorAll(".segment[data-mode]")) {
    button.addEventListener("click", () => {
      setValue("#draft-transport-mode", button.dataset.mode);
      setSegment(".segment[data-mode]", button.dataset.mode, "mode");
      syncDraftDirectionUi();
    });
  }
  for (const button of document.querySelectorAll(".nav-item")) {
    button.addEventListener("click", () => {
      document.querySelectorAll(".nav-item").forEach((item) => item.classList.remove("active"));
      button.classList.add("active");
      document.querySelector(`#${button.dataset.section}`)?.scrollIntoView({ behavior: "smooth", block: "start" });
    });
  }
  syncDraftDirectionUi();
  updateDraftStep();
  for (const selector of ["#draft-source-path", "#draft-target-root", "#draft-device-serial", "#draft-agent-host"]) {
    document.querySelector(selector)?.addEventListener("input", updateDraftStep);
  }
}

async function stageDraft(event) {
  event.preventDefault();
  clearErrors();

  const intent = event.submitter?.dataset.intent || "create";
  const summary = draftSummary();
  const validationErrors = validateDraftSummary(summary);
  if (validationErrors.length) {
    showErrors(validationErrors);
    updateDraftStep();
    return;
  }

  await withBusy(async () => {
    const task = await invoke("create_transfer_task", { draft: summary });
    showToast(intent === "start" ? "任务已创建，正在启动..." : "任务已创建。");
    if (intent === "start") {
      await invoke("start_transfer_task", { taskId: task.task_id });
      showToast("传输已启动。");
    }
  });
  await refreshDashboard({ includeLogs: true, silent: false });
  updateDraftStep();
}

function draftSummary() {
  return {
    source_path: value("#draft-source-path"),
    target_root: value("#draft-target-root"),
    direction: value("#draft-direction") || "PC -> Android",
    transport_mode: value("#draft-transport-mode") || "Dual Track",
    verify_enabled: document.querySelector("#draft-verify")?.checked || false,
    chunk_size_bytes: Math.max(1, Number(value("#draft-chunk-mb") || 8)) * 1024 * 1024,
    max_in_flight_chunks_per_lane: Math.max(1, Number(value("#draft-lane-limit") || 4)),
    device_serial: value("#draft-device-serial") || null,
    agent_host: value("#draft-agent-host") || value("#agent-host-input") || null,
    target_path_policy: "preserve_relative",
  };
}

function validateDraftSummary(summary) {
  const errors = [];
  if (!summary.source_path) errors.push("请选择来源。");
  if (!summary.target_root) errors.push("请填写目标。");
  if (summary.transport_mode === "ADB-only" && !summary.device_serial) errors.push("ADB 需要设备序列号。");
  if (summary.transport_mode === "Wi-Fi-only" && !summary.agent_host) errors.push("Wi-Fi 需要代理地址。");
  if (summary.transport_mode === "Dual Track" && (!summary.device_serial || !summary.agent_host)) {
    errors.push("双通道需要同时具备 ADB 序列号和代理地址。");
  }
  if (summary.direction === "PC -> Android" && summary.target_root.includes("\\")) {
    errors.push("Android 目标路径应类似 /sdcard/Nekotrans。");
  }
  return errors;
}

function updateDraftStep() {
  const summary = draftSummary();
  const errors = validateDraftSummary(summary);
  const sourceInput = document.querySelector("#draft-source-path");
  sourceInput?.classList.toggle("needs-attention", !summary.source_path);
  const targetInput = document.querySelector("#draft-target-root");
  targetInput?.classList.toggle("needs-attention", !summary.target_root);
  if (!summary.source_path) {
    text("#draft-step-text", "请选择要发送的文件或文件夹。");
  } else if (!summary.target_root) {
    text("#draft-step-text", "请填写目标路径。PC → Android 推荐 /sdcard/Nekotrans。");
  } else if (errors.length) {
    text("#draft-step-text", errors[0]);
  } else {
    text("#draft-step-text", "准备就绪，可以点击“创建并启动”。");
  }
}

function syncDraftDirectionUi() {
  const isAndroidToPc = value("#draft-direction") === "Android -> PC";
  const targetInput = document.querySelector("#draft-target-root");
  const targetPicker = document.querySelector("#pick-target-folder-btn");
  if (targetInput) {
    targetInput.placeholder = isAndroidToPc ? "C:\\Users\\me\\Desktop\\NekotransRestore" : "/sdcard/Nekotrans";
  }
  if (targetPicker) {
    targetPicker.disabled = !isAndroidToPc;
  }
}

async function pickDraftSourcePath(pickDirectory) {
  if (!ensureInvokeReady()) return;
  try {
    const path = await invoke("pick_source_path", { pickDirectory });
    if (path) {
      setValue("#draft-source-path", path);
      updateDraftStep();
    }
  } catch (error) {
    showToast(String(error), true);
  }
}

async function pickDraftTargetFolder() {
  if (!ensureInvokeReady()) return;
  if (value("#draft-direction") !== "Android -> PC") return;
  try {
    const path = await invoke("pick_target_folder");
    if (path) {
      setValue("#draft-target-root", path);
      updateDraftStep();
    }
  } catch (error) {
    showToast(String(error), true);
  }
}

async function probeAgentHost() {
  if (!ensureInvokeReady()) return;
  const host = value("#agent-host-input");
  if (!host) {
    showToast("请输入代理地址。", true);
    return;
  }
  await withBusy(async () => {
    const result = await invoke("probe_wifi_agent", { host });
    document.querySelector("#agent-probe-output").textContent = compactJson(result);
    setValue("#draft-agent-host", host);
    showToast("手机端代理可连接。");
  });
}

async function fetchAgentLogs() {
  if (!ensureInvokeReady({ log: true })) return;
  const host = value("#agent-host-input") || value("#draft-agent-host");
  if (!host) {
    showToast("没有代理地址。", true);
    return;
  }
  await withBusy(async () => {
    const result = await invoke("fetch_wifi_agent_logs", { host });
    document.querySelector("#log-export-output").textContent = compactJson(result);
    await refreshLogs();
  });
}

async function refreshLogs() {
  if (!ensureInvokeReady({ log: true })) return;
  try {
    const filters = currentLogFilters();
    const logs = await invoke("list_task_logs_filtered", filters);
    setLogOutput(logs.join("\n") || "暂无日志。");
  } catch (error) {
    const message = `日志读取失败：${String(error)}`;
    setLogOutput(message);
    showToast(message, true);
  }
}

async function exportLogs() {
  if (!ensureInvokeReady({ log: true })) return;
  await withBusy(async () => {
    const path = await invoke("export_logs", currentLogFilters());
    document.querySelector("#log-export-output").textContent = `已导出 ${path}`;
  });
}

function currentLogFilters() {
  return {
    taskId: value("#log-task-filter") || null,
    deviceHost: value("#log-device-filter") || null,
    fromEpochMs: null,
    toEpochMs: null,
    level: value("#log-level-filter") || null,
    text: value("#log-text-filter") || null,
  };
}

async function mutateTask(command, payload) {
  await withBusy(async () => {
    await invoke(command, payload);
  });
  await refreshDashboard({ includeLogs: true, silent: false });
}

async function deleteTaskRecord(taskId) {
  if (!taskId || taskId === "demo-task") return;
  await withBusy(async () => {
    await invoke("delete_transfer_task", { taskId });
    showToast("传输记录已删除。");
  });
  await refreshDashboard({ includeLogs: true, silent: false });
}

async function withBusy(work) {
  if (!ensureInvokeReady()) return;
  if (state.busy) return;
  state.busy = true;
  document.body.classList.add("busy");
  try {
    await work();
  } catch (error) {
    showErrors([String(error)]);
    showToast(String(error), true);
  } finally {
    state.busy = false;
    document.body.classList.remove("busy");
  }
}

function ensureInvokeReady(options = {}) {
  if (invoke) return true;
  const message = "Tauri 桥接未启用，桌面端命令不可用。请重新编译并启动 Windows 端。";
  showErrors([message]);
  if (options.log) {
    setLogOutput(message);
  }
  setRefreshState("不可用");
  showToast(message, true);
  return false;
}

function setLogOutput(message) {
  const output = document.querySelector("#log-output");
  if (output) {
    output.textContent = message;
  }
}

function showErrors(errors) {
  const box = document.querySelector("#draft-errors");
  box.innerHTML = errors.map((error) => `<div>${escapeHtml(error)}</div>`).join("");
  box.hidden = false;
}

function clearErrors() {
  const box = document.querySelector("#draft-errors");
  box.innerHTML = "";
  box.hidden = true;
}

function showToast(message, isError = false) {
  const toast = document.querySelector("#toast");
  toast.textContent = message;
  toast.classList.toggle("error", isError);
  toast.hidden = false;
  clearTimeout(showToast.timer);
  showToast.timer = setTimeout(() => {
    toast.hidden = true;
  }, 3200);
}

function setRefreshState(label) {
  text("#refresh-state", label);
}

function setSegment(selector, selected, dataKey) {
  for (const button of document.querySelectorAll(selector)) {
    button.classList.toggle("active", button.dataset[dataKey] === selected);
  }
}

function text(selector, valueText) {
  const element = document.querySelector(selector);
  if (element) element.textContent = String(valueText);
}

function value(selector) {
  return document.querySelector(selector)?.value?.trim() || "";
}

function setValue(selector, nextValue) {
  const element = document.querySelector(selector);
  if (element) element.value = nextValue;
}

function kebab(valueText) {
  return valueText.replace(/[A-Z]/g, (letter) => `-${letter.toLowerCase()}`);
}

function compactJson(valueObject) {
  return JSON.stringify(valueObject, null, 2);
}

function formatBytes(valueText) {
  const units = ["B", "KB", "MB", "GB", "TB"];
  let size = Number(valueText || 0);
  let unit = 0;
  while (size >= 1024 && unit < units.length - 1) {
    size /= 1024;
    unit += 1;
  }
  return `${size.toFixed(unit === 0 ? 0 : 1)} ${units[unit]}`;
}

function displayState(stateText) {
  return {
    Pending: "待启动",
    Running: "运行中",
    Paused: "已暂停",
    Completed: "已完成",
    Failed: "失败",
    Cancelled: "已取消",
  }[stateText] || stateText;
}

function displayDirection(directionText) {
  return {
    "PC -> Android": "PC → Android",
    "Android -> PC": "Android → PC",
  }[directionText] || directionText;
}

function displayMode(modeText) {
  return {
    "Dual Track": "双通道",
    "ADB-only": "仅 ADB",
    "Wi-Fi-only": "仅 Wi-Fi",
  }[modeText] || modeText;
}

function displayWorkerStage(eventText, messageText, task) {
  const mapped = {
    "dual-same-file-started": "正在准备同文件双通道写入。",
    "dual-same-file-chunk-skipped": "正在校验并跳过已确认的恢复块。",
    "dual-same-file-chunk-pushed": "正在写入同文件分块。",
    "dual-file-completed": "文件已完成，正在收尾。",
    "dual-finalizing": "正在让手机端合并临时文件。",
    "dual-local-verify": "正在 Windows 端计算源文件 BLAKE3。",
    "dual-remote-stat": "正在读取手机端文件大小。",
    "dual-remote-verify": "正在手机端计算目标文件 BLAKE3。",
    "dual-completed": "传输和校验已完成。",
    paused: "链路中断，已停在可恢复边界。",
    failed: "任务失败，请查看错误信息。",
  };
  const stage = mapped[eventText] || messageText || eventText || "";
  if (task?.state === "Running" && task.progress_percent >= 100 && !stage) {
    return "数据已写完，正在完成文件收尾或校验。";
  }
  return stage;
}

function escapeHtml(valueText) {
  return String(valueText ?? "")
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#039;");
}

loadDashboard();
