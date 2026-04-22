const invoke = window.__TAURI__?.core?.invoke;

async function loadDashboard() {
  const dashboard = invoke
    ? await invoke("bootstrap_dashboard")
    : {
        app_name: "Nekotrans",
        transport_modes: ["ADB-only", "Wi-Fi-only", "Dual Track"],
        devices: [
          {
            id: "R3CN30ABCDEF",
            label: "Pixel 7 Pro",
            lane_mode: "ADB (USB) + TCP Candidate",
            agent_host: "192.168.31.20",
            adb_ready: true,
            wifi_ready: true,
            transfer_ready: true,
            protocol_version: "adb-preflight",
            status_text: "adb shell ready",
            platform_text: "Google / Android 14 / SDK 34",
            preflight_checks: [
              { label: "ADB Link", passed: true, detail: "status=device" },
              { label: "Shell Ready", passed: true, detail: "device" },
              { label: "Android SDK", passed: true, detail: "Android 14 / SDK 34" },
              { label: "CPU ABI", passed: true, detail: "arm64-v8a" },
              { label: "Wi-Fi Candidate", passed: true, detail: "5555" },
              { label: "Agent Package", passed: false, detail: "not installed" },
              { label: "Remote /sdcard", passed: true, detail: "reachable" },
              { label: "Probe Error", passed: true, detail: "none" },
            ],
          },
        ],
        tasks: [
          {
            task_id: "demo-task",
            state: "Pending",
            direction: "PC -> Android",
            transport_mode: "Dual Track",
            verify_enabled: false,
            total_files: 3,
            total_bytes: 114294784,
            committed_bytes: 0,
            progress_percent: 0,
            adb_bytes: 0,
            wifi_bytes: 0,
            completed_chunks: 0,
          },
        ],
        recoverable_tasks: ["demo-task"],
        sample_logs: [
          "{\"scope\":\"audit\",\"message\":\"desktop shell started\"}",
          "{\"scope\":\"device\",\"message\":\"adb discovery completed\"}",
        ],
      };

  const template = invoke
    ? await invoke("sample_task_template")
    : {
        task_id: "demo-task",
        direction: "PC -> Android",
        transport_mode: "Dual Track",
        verify_enabled: false,
        chunk_size_mb: 8,
      };

  renderDevices(dashboard.devices);
  renderConnectionOverview(dashboard);
  renderModes(dashboard.transport_modes);
  renderDraftTransportModes(dashboard.transport_modes);
  renderTemplate(template);
  renderTasks(dashboard.tasks);
  renderRecoverables(dashboard.recoverable_tasks);
  renderAdbTransfers(invoke ? await invoke("list_adb_transfers") : []);
  document.querySelector("#log-output").textContent = dashboard.sample_logs.join("\n");
  bindActions();
}

function renderDevices(devices) {
  const host = document.querySelector("#device-list");
  host.innerHTML = "";
  if (!devices.length) {
    host.innerHTML = `<div class="empty-state">No attached ADB devices were discovered.</div>`;
    return;
  }

  for (const device of devices) {
    const card = document.createElement("div");
    card.className = "device-card";
    const installDisabled = !device.adb_ready ? "disabled" : "";
    const agentHost = device.agent_host || "";
    const wifiDisabled = agentHost && device.wifi_ready ? "" : "disabled";
    const dualDisabled = device.adb_ready && agentHost ? "" : "disabled";
    card.innerHTML = `
      <div class="device-body">
        <div>
          <strong>${escapeHtml(device.label)}</strong>
          <p>${escapeHtml(device.id)} / ${escapeHtml(device.lane_mode)} / ${escapeHtml(device.platform_text)}</p>
          <p>${escapeHtml(device.status_text)}</p>
        </div>
        <div class="status-grid">
          <span class="${device.adb_ready ? "good" : "bad"}">ADB</span>
          <span class="${device.wifi_ready ? "good" : "warn"}">Wi-Fi Candidate</span>
          <span class="${device.transfer_ready ? "good" : "warn"}">Transfer Ready</span>
          <button class="inline-action" data-install="${device.id}" ${installDisabled}>Install Agent</button>
          <button class="inline-action" data-push-docs="${device.id}" ${installDisabled}>Push Docs</button>
        </div>
      </div>
      <div class="lane-pair">
        <div>
          <span>Desktop fills</span>
          <strong>${escapeHtml(device.id)}</strong>
        </div>
        <div>
          <span>Android agent</span>
          <strong>${agentHost ? escapeHtml(`${agentHost}:38997`) : "Not discovered"}</strong>
        </div>
      </div>
      <div class="action-row">
        <button class="inline-action" data-use-device="${device.id}" data-agent-host="${escapeHtml(agentHost)}" data-use-mode="Dual Track" ${dualDisabled}>Use Dual</button>
        <button class="inline-action" data-use-device="${device.id}" data-agent-host="${escapeHtml(agentHost)}" data-use-mode="ADB-only" ${installDisabled}>Use ADB</button>
        <button class="inline-action" data-use-device="${device.id}" data-agent-host="${escapeHtml(agentHost)}" data-use-mode="Wi-Fi-only" ${wifiDisabled}>Use Wi-Fi</button>
      </div>
      <div class="check-grid">
        ${device.preflight_checks
          .map(
            (check) => `
              <div class="check-card">
                <span class="${check.passed ? "good" : "warn"}">${escapeHtml(check.label)}</span>
                <strong>${escapeHtml(check.detail)}</strong>
              </div>
            `,
          )
          .join("")}
      </div>
    `;
    host.appendChild(card);
  }

  for (const button of host.querySelectorAll("[data-install]")) {
    button.addEventListener("click", async (event) => {
      const serial = event.currentTarget.getAttribute("data-install");
      await installAgent(serial);
    });
  }

  for (const button of host.querySelectorAll("[data-push-docs]")) {
    button.addEventListener("click", async (event) => {
      const serial = event.currentTarget.getAttribute("data-push-docs");
      await pushDocs(serial);
    });
  }

  for (const button of host.querySelectorAll("[data-use-device]")) {
    button.addEventListener("click", (event) => {
      const target = event.currentTarget;
      fillDraftFromDevice(
        target.getAttribute("data-use-device"),
        target.getAttribute("data-agent-host"),
        target.getAttribute("data-use-mode"),
      );
    });
  }
}

function renderConnectionOverview(dashboard) {
  const devices = dashboard.devices || [];
  const tasks = dashboard.tasks || [];
  const dualReady = devices.filter((device) => device.adb_ready && device.agent_host).length;
  const activeTasks = tasks.filter((task) => ["Pending", "Running", "Paused", "Failed"].includes(task.state)).length;
  document.querySelector("#metric-devices").textContent = String(devices.length);
  document.querySelector("#metric-dual-ready").textContent = String(dualReady);
  document.querySelector("#metric-active-tasks").textContent = String(activeTasks);

  const overview = document.querySelector("#link-overview");
  if (!overview) {
    return;
  }
  const bestDevice = devices.find((device) => device.adb_ready && device.agent_host) || devices[0];
  if (!bestDevice) {
    overview.innerHTML = `<div class="empty-state">Connect an Android phone with USB debugging enabled, then refresh links.</div>`;
    return;
  }
  overview.innerHTML = `
    <div class="link-node">
      <span>Windows</span>
      <strong>Desktop scheduler</strong>
    </div>
    <div class="link-arrow">ADB + Wi-Fi</div>
    <div class="link-node">
      <span>Android</span>
      <strong>${escapeHtml(bestDevice.label)}</strong>
      <small>${escapeHtml(bestDevice.agent_host ? `${bestDevice.agent_host}:38997` : bestDevice.id)}</small>
    </div>
  `;
}

function fillDraftFromDevice(serial, agentHost, mode) {
  const serialInput = document.querySelector("#draft-device-serial");
  const agentInput = document.querySelector("#draft-agent-host");
  const manualAgentInput = document.querySelector("#agent-host-input");
  const modeSelect = document.querySelector("#draft-transport-mode");
  const directionSelect = document.querySelector("#draft-direction");
  const targetInput = document.querySelector("#draft-target-root");
  const verifyInput = document.querySelector("#draft-verify");
  const output = document.querySelector("#draft-summary");

  if (serialInput) {
    serialInput.value = serial || "";
  }
  if (agentInput) {
    agentInput.value = agentHost || "";
  }
  if (manualAgentInput && agentHost) {
    manualAgentInput.value = agentHost;
  }
  if (modeSelect && mode) {
    modeSelect.value = mode;
  }
  if (directionSelect) {
    directionSelect.value = "PC -> Android";
  }
  if (targetInput && !targetInput.value.trim()) {
    targetInput.value = "/sdcard/Nekotrans";
  }
  if (verifyInput) {
    verifyInput.checked = true;
  }
  syncDraftDirectionUi();
  if (output) {
    output.textContent = JSON.stringify(
      { selected_device: serial, agent_host: agentHost || null, transport_mode: mode },
      null,
      2,
    );
  }
}

async function installAgent(serial) {
  if (!invoke) {
    return;
  }

  try {
    const result = await invoke("install_agent", { serial });
    console.log(result);
  } catch (error) {
    console.error(error);
  }

  await refreshDashboard();
}

async function pushDocs(serial) {
  if (!invoke) {
    return;
  }

  try {
    const result = await invoke("start_adb_docs_push", { serial });
    console.log(result);
  } catch (error) {
    console.error(error);
  }

  await refreshDashboard();
}

async function pauseAdbTransfer(taskId) {
  if (!invoke) {
    return;
  }

  try {
    await invoke("pause_adb_transfer", { taskId });
  } catch (error) {
    console.error(error);
  }

  await refreshDashboard();
}

async function probeAgentHost() {
  if (!invoke) {
    return;
  }

  const input = document.querySelector("#agent-host-input");
  const output = document.querySelector("#agent-probe-output");
  const host = input?.value?.trim();
  if (!host) {
    output.textContent = "Enter a LAN IP first.";
    return;
  }

  output.textContent = "Probing...";
  try {
    const result = await invoke("probe_wifi_agent", { host });
    output.textContent = JSON.stringify(result, null, 2);
  } catch (error) {
    output.textContent = String(error);
  }
}

async function startAgentTask() {
  if (!invoke) {
    return;
  }

  const input = document.querySelector("#agent-host-input");
  const output = document.querySelector("#agent-probe-output");
  const host = input?.value?.trim();
  if (!host) {
    output.textContent = "Enter a LAN IP first.";
    return;
  }

  const taskId = `wifi-skeleton-${Date.now()}`;
  output.textContent = "Starting agent task skeleton...";
  try {
    const result = await invoke("start_wifi_agent_task", { host, taskId });
    output.textContent = JSON.stringify(result, null, 2);
  } catch (error) {
    output.textContent = String(error);
  }
}

async function mutateAgentTask(command) {
  if (!invoke) {
    return;
  }

  const input = document.querySelector("#agent-host-input");
  const output = document.querySelector("#agent-probe-output");
  const host = input?.value?.trim();
  if (!host) {
    output.textContent = "Enter a LAN IP first.";
    return;
  }

  output.textContent = "Sending command...";
  try {
    const result = await invoke(command, { host });
    output.textContent = JSON.stringify(result, null, 2);
  } catch (error) {
    output.textContent = String(error);
  }
}

async function pushAgentSampleChunk() {
  if (!invoke) {
    return;
  }

  const input = document.querySelector("#agent-host-input");
  const output = document.querySelector("#agent-probe-output");
  const host = input?.value?.trim();
  if (!host) {
    output.textContent = "Enter a LAN IP first.";
    return;
  }

  output.textContent = "Pushing sample chunk...";
  try {
    const result = await invoke("push_wifi_agent_sample_chunk", { host });
    output.textContent = JSON.stringify(result, null, 2);
  } catch (error) {
    output.textContent = String(error);
  }
}

async function resumeAdbTransfer(taskId) {
  if (!invoke) {
    return;
  }

  try {
    await invoke("resume_adb_transfer", { taskId });
  } catch (error) {
    console.error(error);
  }

  await refreshDashboard();
}

function renderAdbTransfers(transfers) {
  const host = document.querySelector("#adb-transfer-list");
  if (!host) {
    return;
  }

  host.innerHTML = "";
  if (!transfers.length) {
    host.innerHTML = `<div class="empty-state">No ADB worker transfers yet. Use Push Docs on a ready device.</div>`;
    return;
  }

  for (const transfer of transfers) {
    const totalFiles = transfer.total_files || 1;
    const completedFiles = transfer.pushed_files + transfer.skipped_files;
    const progress = Math.min(100, Math.round((completedFiles * 100) / totalFiles));
    const canPause = transfer.state === "Running";
    const canResume = transfer.state === "Paused";
    const card = document.createElement("div");
    card.className = "task-card";
    card.innerHTML = `
      <div class="task-header">
        <div>
          <strong>${transfer.task_id}</strong>
          <p>${transfer.serial} / ${transfer.relative_path || "preparing"} / ${transfer.remote_path}</p>
        </div>
        <span class="badge ${transfer.state === "Completed" ? "good" : transfer.state === "Paused" ? "accent" : transfer.state === "Failed" ? "bad" : ""}">${transfer.state}</span>
      </div>
      <div class="progress-bar">
        <div class="progress-fill" style="width:${progress}%"></div>
      </div>
      <div class="task-stats">
        <span>Files ${completedFiles}/${transfer.total_files}</span>
        <span>Chunks +${transfer.pushed_chunks} / skip ${transfer.skipped_chunks}</span>
        <span>Pushed ${formatBytes(transfer.bytes_pushed)}</span>
        <span>${transfer.last_event}</span>
      </div>
      <p class="muted-line">${transfer.last_message}</p>
      <div class="action-row">
        <button class="inline-action" data-pause-adb="${transfer.task_id}" ${canPause ? "" : "disabled"}>Pause Worker</button>
        <button class="inline-action" data-resume-adb="${transfer.task_id}" ${canResume ? "" : "disabled"}>Resume Worker</button>
      </div>
    `;
    host.appendChild(card);
  }

  for (const button of host.querySelectorAll("[data-pause-adb]")) {
    button.addEventListener("click", async (event) => {
      const taskId = event.currentTarget.getAttribute("data-pause-adb");
      await pauseAdbTransfer(taskId);
    });
  }

  for (const button of host.querySelectorAll("[data-resume-adb]")) {
    button.addEventListener("click", async (event) => {
      const taskId = event.currentTarget.getAttribute("data-resume-adb");
      await resumeAdbTransfer(taskId);
    });
  }
}

function renderRecoverables(taskIds) {
  const host = document.querySelector("#recoverable-list");
  host.innerHTML = "";
  if (!taskIds.length) {
    host.innerHTML = `<div class="empty-state">No recoverable checkpoints found.</div>`;
    return;
  }

  for (const taskId of taskIds) {
    const row = document.createElement("div");
    row.className = "recoverable-row";
    row.innerHTML = `
      <strong>${taskId}</strong>
      <button class="inline-action" data-task-id="${taskId}">Recover</button>
    `;
    host.appendChild(row);
  }

  for (const button of host.querySelectorAll("[data-task-id]")) {
    button.addEventListener("click", async (event) => {
      const taskId = event.currentTarget.getAttribute("data-task-id");
      await mutateTask("recover_task", { taskId });
    });
  }
}

function renderModes(modes) {
  const host = document.querySelector("#mode-list");
  host.innerHTML = "";
  for (const mode of modes) {
    const chip = document.createElement("span");
    chip.className = "chip";
    chip.textContent = mode;
    host.appendChild(chip);
  }
}

function renderDraftTransportModes(modes) {
  const select = document.querySelector("#draft-transport-mode");
  if (!select || !modes?.length) {
    return;
  }

  select.innerHTML = "";
  for (const mode of modes) {
    const option = document.createElement("option");
    option.value = mode;
    option.textContent = mode;
    select.appendChild(option);
  }
}

function renderTemplate(template) {
  const host = document.querySelector("#task-template");
  host.innerHTML = `
    <div class="template-row"><span>Task ID</span><strong>${template.task_id}</strong></div>
    <div class="template-row"><span>Direction</span><strong>${template.direction}</strong></div>
    <div class="template-row"><span>Transport</span><strong>${template.transport_mode}</strong></div>
    <div class="template-row"><span>Verify</span><strong>${template.verify_enabled ? "Enabled" : "Disabled"}</strong></div>
    <div class="template-row"><span>Chunk</span><strong>${template.chunk_size_mb} MB</strong></div>
  `;
}

function renderTasks(tasks) {
  const host = document.querySelector("#task-list");
  host.innerHTML = "";
  for (const task of tasks) {
    const canStart = ["Pending", "Paused", "Failed"].includes(task.state);
    const canPause = task.state === "Running";
    const canRetry = task.state === "Failed" || task.state === "Paused";
    const canCancel = !["Completed", "Cancelled"].includes(task.state);
    const card = document.createElement("div");
    card.className = "task-card";
    card.innerHTML = `
      <div class="task-header">
        <div>
          <strong>${task.task_id}</strong>
          <p>${task.direction} / ${task.transport_mode} / ${task.total_files} files</p>
        </div>
        <span class="badge ${task.state === "Completed" ? "good" : task.state === "Paused" ? "accent" : ""}">${task.state}</span>
      </div>
      <div class="progress-bar">
        <div class="progress-fill" style="width:${task.progress_percent}%"></div>
      </div>
      <div class="task-stats">
        <span>Progress ${task.progress_percent}%</span>
        <span>Chunk ${task.completed_chunks}</span>
        <span>ADB ${formatBytes(task.adb_bytes)}</span>
        <span>Wi-Fi ${formatBytes(task.wifi_bytes)}</span>
      </div>
      ${task.last_error ? `<p class="muted-line error-line">${task.last_error}</p>` : ""}
      <div class="action-row">
        <button class="inline-action" data-start-task="${task.task_id}" ${canStart ? "" : "disabled"}>Start</button>
        <button class="inline-action" data-pause-task="${task.task_id}" ${canPause ? "" : "disabled"}>Pause</button>
        <button class="inline-action" data-resume-task="${task.task_id}" ${task.state === "Paused" ? "" : "disabled"}>Resume</button>
        <button class="inline-action" data-retry-task="${task.task_id}" ${canRetry ? "" : "disabled"}>Retry</button>
        <button class="inline-action" data-cancel-task="${task.task_id}" ${canCancel ? "" : "disabled"}>Cancel</button>
      </div>
    `;
    host.appendChild(card);
  }

  for (const button of host.querySelectorAll("[data-start-task]")) {
    button.addEventListener("click", async (event) => {
      await startTransferTask(event.currentTarget.getAttribute("data-start-task"));
    });
  }
  for (const button of host.querySelectorAll("[data-pause-task]")) {
    button.addEventListener("click", async (event) => {
      await mutateTask("pause_transfer_task", { taskId: event.currentTarget.getAttribute("data-pause-task") });
    });
  }
  for (const button of host.querySelectorAll("[data-resume-task]")) {
    button.addEventListener("click", async (event) => {
      await resumeTransferTask(event.currentTarget.getAttribute("data-resume-task"));
    });
  }
  for (const button of host.querySelectorAll("[data-retry-task]")) {
    button.addEventListener("click", async (event) => {
      await mutateTask("retry_transfer_task", { taskId: event.currentTarget.getAttribute("data-retry-task") });
    });
  }
  for (const button of host.querySelectorAll("[data-cancel-task]")) {
    button.addEventListener("click", async (event) => {
      await mutateTask("cancel_transfer_task", { taskId: event.currentTarget.getAttribute("data-cancel-task") });
    });
  }
}

async function startTransferTask(taskId) {
  if (!invoke) {
    return;
  }

  try {
    await invoke("start_transfer_task", { taskId });
  } catch (error) {
    console.error(error);
  }

  await refreshDashboard();
}

async function resumeTransferTask(taskId) {
  if (!invoke) {
    return;
  }

  try {
    await invoke("resume_transfer_task", { taskId });
  } catch (error) {
    console.error(error);
  }

  await refreshDashboard();
}

function bindActions() {
  document
    .querySelector("#create-local-btn")
    ?.addEventListener("click", () => mutateTask("create_demo_local_task"));
  document
    .querySelector("#advance-btn")
    ?.addEventListener("click", () => mutateTask("tick_demo_task"));
  document
    .querySelector("#pause-btn")
    ?.addEventListener("click", () => mutateTask("pause_demo_task"));
  document
    .querySelector("#resume-btn")
    ?.addEventListener("click", () => mutateTask("resume_demo_task"));
  document.querySelector("#refresh-devices-btn")?.addEventListener("click", () => refreshDashboard());
  document.querySelector("#probe-agent-btn")?.addEventListener("click", () => probeAgentHost());
  document.querySelector("#start-agent-task-btn")?.addEventListener("click", () => startAgentTask());
  document
    .querySelector("#pause-agent-task-btn")
    ?.addEventListener("click", () => mutateAgentTask("pause_wifi_agent_task"));
  document
    .querySelector("#resume-agent-task-btn")
    ?.addEventListener("click", () => mutateAgentTask("resume_wifi_agent_task"));
  document
    .querySelector("#push-agent-sample-btn")
    ?.addEventListener("click", () => pushAgentSampleChunk());
  document.querySelector("#refresh-logs-btn")?.addEventListener("click", () => refreshLogs());
  document.querySelector("#export-logs-btn")?.addEventListener("click", () => exportLogs());
  document.querySelector("#fetch-agent-logs-btn")?.addEventListener("click", () => fetchAgentLogs());
  document.querySelector("#draft-form")?.addEventListener("submit", stageDraft);
  document.querySelector("#draft-direction")?.addEventListener("change", syncDraftDirectionUi);
  document.querySelector("#draft-transport-mode")?.addEventListener("change", syncDraftDirectionUi);
  document.querySelector("#pick-source-file-btn")?.addEventListener("click", () => pickDraftSourcePath(false));
  document.querySelector("#pick-source-folder-btn")?.addEventListener("click", () => pickDraftSourcePath(true));
  document.querySelector("#pick-target-folder-btn")?.addEventListener("click", () => pickDraftTargetFolder());
  syncDraftDirectionUi();
}

function syncDraftDirectionUi() {
  const direction = document.querySelector("#draft-direction")?.value || "PC -> Android";
  const transportMode = document.querySelector("#draft-transport-mode")?.value || "Dual Track";
  const targetInput = document.querySelector("#draft-target-root");
  const targetHint = document.querySelector("#draft-target-hint");
  const targetPicker = document.querySelector("#pick-target-folder-btn");
  const laneHint = document.querySelector("#draft-lane-hint");
  const isAndroidToPc = direction === "Android -> PC";

  if (targetInput) {
    targetInput.placeholder = isAndroidToPc
      ? "C:\\Users\\me\\Desktop\\NekotransRestore"
      : "/sdcard/Nekotrans/Pictures";
  }
  if (targetHint) {
    targetHint.textContent = isAndroidToPc
      ? "Android -> PC uses a local Windows target folder, so the picker can fill this field."
      : "PC -> Android writes to an Android path; enter the remote target root manually.";
  }
  if (targetPicker) {
    targetPicker.disabled = !isAndroidToPc;
    targetPicker.title = isAndroidToPc ? "" : "Local folder picker only applies to Android -> PC tasks.";
  }
  if (laneHint) {
    const directionHint = isAndroidToPc
      ? "Android -> PC uses a local Windows target folder."
      : "PC -> Android uses a remote Android target root.";
    const modeHint =
      transportMode === "ADB-only"
        ? "ADB-only requires an ADB serial."
        : transportMode === "Wi-Fi-only"
          ? "Wi-Fi-only requires an agent host."
          : "Dual Track uses both lanes when an ADB serial and agent host are available; otherwise it degrades to the available lane.";
    laneHint.textContent = `${directionHint} ${modeHint}`;
  }
}

function validateDraftSummary(summary) {
  const errors = [];
  if (!summary.source_path) {
    errors.push("Source path is required.");
  }
  if (!summary.target_root) {
    errors.push("Target root is required.");
  }
  if (summary.transport_mode === "ADB-only" && !summary.device_serial) {
    errors.push("ADB-only tasks require an ADB serial.");
  }
  if (summary.transport_mode === "Wi-Fi-only" && !summary.agent_host) {
    errors.push("Wi-Fi-only tasks require an agent host.");
  }
  if (summary.transport_mode === "Dual Track" && !summary.device_serial && !summary.agent_host) {
    errors.push("Dual Track currently needs at least one usable lane: ADB serial or agent host.");
  }
  if (summary.direction === "PC -> Android" && summary.target_root.includes("\\")) {
    errors.push("PC -> Android target root should look like an Android path such as /sdcard/Nekotrans.");
  }
  if (summary.direction === "Android -> PC" && !summary.target_root.match(/^[A-Za-z]:\\/)) {
    errors.push("Android -> PC target root should be a local Windows folder.");
  }
  return errors;
}

async function pickDraftSourcePath(pickDirectory) {
  if (!invoke) {
    return;
  }

  try {
    const path = await invoke("pick_source_path", { pickDirectory });
    if (path) {
      document.querySelector("#draft-source-path").value = path;
    }
  } catch (error) {
    document.querySelector("#draft-summary").textContent = String(error);
  }
}

async function pickDraftTargetFolder() {
  if (!invoke) {
    return;
  }

  if ((document.querySelector("#draft-direction")?.value || "PC -> Android") !== "Android -> PC") {
    document.querySelector("#draft-summary").textContent =
      "Target folder picker is available for Android -> PC tasks only.";
    return;
  }

  try {
    const path = await invoke("pick_target_folder");
    if (path) {
      document.querySelector("#draft-target-root").value = path;
    }
  } catch (error) {
    document.querySelector("#draft-summary").textContent = String(error);
  }
}

function currentLogFilters() {
  const fromValue = document.querySelector("#log-from-filter")?.value;
  const toValue = document.querySelector("#log-to-filter")?.value;
  return {
    taskId: document.querySelector("#log-task-filter")?.value.trim() || null,
    deviceHost: document.querySelector("#log-device-filter")?.value.trim() || null,
    fromEpochMs: fromValue ? new Date(fromValue).getTime() : null,
    toEpochMs: toValue ? new Date(toValue).getTime() : null,
    level: document.querySelector("#log-level-filter")?.value || null,
    text: document.querySelector("#log-text-filter")?.value.trim() || null,
  };
}

async function refreshLogs() {
  if (!invoke) {
    return;
  }

  const filters = currentLogFilters();
  try {
    const logs = await invoke("list_task_logs_filtered", filters);
    document.querySelector("#log-output").textContent = logs.join("\n");
  } catch (error) {
    document.querySelector("#log-export-output").textContent = String(error);
  }
}

async function exportLogs() {
  if (!invoke) {
    return;
  }

  const filters = currentLogFilters();
  const output = document.querySelector("#log-export-output");
  try {
    const path = await invoke("export_logs", filters);
    output.textContent = `Exported ${path}`;
  } catch (error) {
    output.textContent = String(error);
  }
}

async function fetchAgentLogs() {
  if (!invoke) {
    return;
  }

  const host = document.querySelector("#agent-host-input")?.value.trim();
  const output = document.querySelector("#log-export-output");
  if (!host) {
    output.textContent = "Enter a LAN IP first.";
    return;
  }

  try {
    const result = await invoke("fetch_wifi_agent_logs", { host });
    output.textContent = JSON.stringify(result, null, 2);
  } catch (error) {
    output.textContent = String(error);
  }
}

async function stageDraft(event) {
  event.preventDefault();

  const summary = {
    source_path: document.querySelector("#draft-source-path")?.value.trim() || "",
    target_root: document.querySelector("#draft-target-root")?.value.trim() || "",
    direction: document.querySelector("#draft-direction")?.value || "PC -> Android",
    transport_mode: document.querySelector("#draft-transport-mode")?.value || "Dual Track",
    verify_enabled: document.querySelector("#draft-verify")?.checked || false,
    chunk_size_bytes: Math.max(1, Number(document.querySelector("#draft-chunk-mb")?.value || 8)) * 1024 * 1024,
    max_in_flight_chunks_per_lane: Math.max(1, Number(document.querySelector("#draft-lane-limit")?.value || 4)),
    device_serial: document.querySelector("#draft-device-serial")?.value.trim() || null,
    agent_host:
      document.querySelector("#draft-agent-host")?.value.trim() ||
      document.querySelector("#agent-host-input")?.value.trim() ||
      null,
    target_path_policy: "preserve_relative",
    staged_at: new Date().toISOString(),
  };

  const output = document.querySelector("#draft-summary");
  const validationErrors = validateDraftSummary(summary);
  if (validationErrors.length) {
    output.textContent = JSON.stringify({ draft: summary, validation_errors: validationErrors }, null, 2);
    return;
  }
  output.textContent = JSON.stringify(summary, null, 2);

  if (!invoke) {
    return;
  }

  try {
    const task = await invoke("create_transfer_task", { draft: summary });
    output.textContent = JSON.stringify({ draft: summary, task }, null, 2);
    await refreshDashboard();
  } catch (error) {
    output.textContent = JSON.stringify({ draft: summary, error: String(error) }, null, 2);
  }
}

async function mutateTask(command, payload = undefined) {
  if (!invoke) {
    return;
  }

  try {
    await invoke(command, payload);
  } catch (error) {
    console.error(error);
  }

  await refreshDashboard();
}

async function refreshDashboard() {
  if (!invoke) {
    return;
  }

  const [dashboard, logs, adbTransfers] = await Promise.all([
    invoke("bootstrap_dashboard"),
    invoke("list_task_logs_filtered", currentLogFilters()),
    invoke("list_adb_transfers"),
  ]);
  renderDevices(dashboard.devices);
  renderConnectionOverview(dashboard);
  renderTasks(dashboard.tasks);
  renderRecoverables(dashboard.recoverable_tasks);
  renderAdbTransfers(adbTransfers);
  document.querySelector("#log-output").textContent = logs.join("\n");
}

function formatBytes(value) {
  const units = ["B", "KB", "MB", "GB"];
  let size = value;
  let unitIndex = 0;
  while (size >= 1024 && unitIndex < units.length - 1) {
    size /= 1024;
    unitIndex += 1;
  }
  return `${size.toFixed(unitIndex === 0 ? 0 : 1)} ${units[unitIndex]}`;
}

function escapeHtml(value) {
  return String(value ?? "")
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#039;");
}

loadDashboard();
setInterval(() => {
  if (invoke) {
    refreshDashboard();
  }
}, 1500);
