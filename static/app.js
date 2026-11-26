(() => {
  const sessionListEl = document.getElementById('session-list');
  const apiInfoEl = document.getElementById('api-info');
  const activeTitleEl = document.getElementById('active-session-title');
  const activeMetaEl = document.getElementById('active-session-meta');
  const termContainer = document.getElementById('terminal-container');

  const btnNewSession = document.getElementById('btn-new-session');
  const btnSendEnter = document.getElementById('btn-send-enter');
  const btnSnapshot = document.getElementById('btn-snapshot');
  const btnDeleteSession = document.getElementById('btn-delete-session');

  const dialog = document.getElementById('new-session-dialog');
  const newSessionForm = document.getElementById('new-session-form');
  const btnCancelModal = document.getElementById('btn-cancel-modal');

  const apiKeyInput = document.getElementById('api-key-input');
  const apiModelInput = document.getElementById('api-model-input');
  const apiPromptInput = document.getElementById('api-prompt-input');
  const apiStreamCheckbox = document.getElementById('api-stream-checkbox');
  const apiTestForm = document.getElementById('api-test-form');
  const apiTestOutput = document.getElementById('api-test-output');
  const btnSendApi = document.getElementById('btn-send-api');

  const savedKey = localStorage.getItem('gproxy_api_key');
  if (savedKey) apiKeyInput.value = savedKey;
  apiKeyInput.addEventListener('change', () => {
    localStorage.setItem('gproxy_api_key', apiKeyInput.value.trim());
  });

  let sessions = [];
  let activeSessionId = null;
  let ws = null;
  let term = null;
  let fitAddon = null;
  let reconnectTimer = null;

  function ensureTerminal() {
    if (term) return;
    termContainer.innerHTML = '';
    term = new window.Terminal({
      cursorBlink: true,
      fontSize: 13,
      fontFamily: 'Menlo, Monaco, "Cascadia Mono", Consolas, monospace',
      theme: { background: '#000000' },
      scrollback: 5000,
    });
    fitAddon = new window.FitAddon.FitAddon();
    term.loadAddon(fitAddon);
    term.open(termContainer);
    fitAddon.fit();

    term.onData((data) => {
      if (ws && ws.readyState === WebSocket.OPEN) {
        ws.send(JSON.stringify({ type: 'input', data }));
      }
    });

    term.onResize(({ cols, rows }) => {
      if (ws && ws.readyState === WebSocket.OPEN) {
        ws.send(JSON.stringify({ type: 'resize', cols, rows }));
      }
    });

    window.addEventListener('resize', () => {
      if (fitAddon) fitAddon.fit();
    });
  }

  function connectSession(sessionId) {
    activeSessionId = sessionId;
    if (reconnectTimer) {
      clearTimeout(reconnectTimer);
      reconnectTimer = null;
    }
    if (ws) {
      ws.onclose = null;
      ws.close();
      ws = null;
    }

    ensureTerminal();
    term.reset();
    fitAddon.fit();

    const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
    const url = `${proto}//${location.host}/ws/terminal/${encodeURIComponent(sessionId)}`;
    ws = new WebSocket(url);
    ws.binaryType = 'arraybuffer';

    ws.onopen = () => {
      if (fitAddon) {
        fitAddon.fit();
        ws.send(JSON.stringify({ type: 'resize', cols: term.cols, rows: term.rows }));
      }
    };

    ws.onmessage = (evt) => {
      if (typeof evt.data === 'string') {
        try {
          const msg = JSON.parse(evt.data);
          if (msg.type === 'connected') {
            updateToolbar();
          }
        } catch (_) {}
        return;
      }
      if (evt.data instanceof ArrayBuffer) {
        term.write(new Uint8Array(evt.data));
      }
    };

    ws.onclose = () => {
      if (activeSessionId === sessionId) {
        reconnectTimer = setTimeout(() => {
          if (activeSessionId === sessionId) connectSession(sessionId);
        }, 1500);
      }
    };

    renderSessions();
    updateToolbar();
  }

  function updateToolbar() {
    const s = sessions.find((item) => item.id === activeSessionId);
    if (!s) {
      activeTitleEl.textContent = '未选择终端会话';
      activeMetaEl.textContent = '';
      btnSendEnter.disabled = true;
      btnSnapshot.disabled = true;
      btnDeleteSession.disabled = true;
      return;
    }
    activeTitleEl.textContent = s.name;
    activeMetaEl.textContent = `model=${s.model_alias} · agent=${s.detected_agent} · id=${s.id}`;
    if (!apiModelInput.value.trim()) {
      apiModelInput.placeholder = `默认: ${s.model_alias}`;
    }
    btnSendEnter.disabled = false;
    btnSnapshot.disabled = false;
    btnDeleteSession.disabled = false;
  }

  function renderSessions() {
    if (!sessions.length) {
      sessionListEl.innerHTML = '<div class="empty-hint">暂无会话，点击右上角新建</div>';
      return;
    }
    sessionListEl.innerHTML = '';
    for (const s of sessions) {
      const card = document.createElement('div');
      card.className = 'session-item' + (s.id === activeSessionId ? ' active' : '');
      const statusClass = !s.alive ? 'badge-dead' : s.busy ? 'badge-busy' : 'badge-idle';
      const statusText = !s.alive ? 'dead' : s.busy ? 'busy' : 'idle';
      const ownerLabel = s.owner_key_id ? ` · owner=${escapeHtml(s.owner_key_id)}` : '';
      const managedTag = s.managed ? ' [auto]' : '';
      const activeModel = s.detected_model ? ` · active=${escapeHtml(s.detected_model)}` : '';
      card.innerHTML = `
        <div class="session-row">
          <span class="session-name"></span>
          <span class="badge ${statusClass}">${statusText}</span>
        </div>
        <div class="session-sub">model: ${escapeHtml(s.model_alias)} · ${escapeHtml(s.detected_agent)}${activeModel} · turns=${s.turn_count || 0}${managedTag}${ownerLabel}</div>
      `;
      card.querySelector('.session-name').textContent = s.name;
      card.addEventListener('click', () => connectSession(s.id));
      sessionListEl.appendChild(card);
    }
  }

  async function refreshSessions() {
    try {
      const resp = await fetch('/api/sessions');
      const data = await resp.json();
      sessions = data.sessions || [];
      if (activeSessionId && !sessions.some((s) => s.id === activeSessionId)) {
        activeSessionId = null;
      }
      if (!activeSessionId && sessions.length > 0) {
        connectSession(sessions[0].id);
      } else {
        renderSessions();
        updateToolbar();
      }
    } catch (err) {
      console.error('refreshSessions failed', err);
    }
  }

  async function refreshInfo() {
    try {
      const resp = await fetch('/api/info');
      const info = await resp.json();
      apiInfoEl.textContent =
        `POST ${location.origin}/v1/chat/completions | Key: ${info.api_key_masked} | Timeout: ${info.request_timeout_secs}s`;
    } catch (_) {
      apiInfoEl.textContent = '无法读取服务信息';
    }
  }

  btnNewSession.addEventListener('click', () => dialog.showModal());
  btnCancelModal.addEventListener('click', () => dialog.close());

  newSessionForm.addEventListener('submit', async (e) => {
    e.preventDefault();
    const payload = {
      name: document.getElementById('new-name').value.trim(),
      model_alias: document.getElementById('new-model').value.trim() || null,
      cwd: document.getElementById('new-cwd').value.trim() || null,
      startup_command: document.getElementById('new-cmd').value.trim() || null,
      agent_kind: document.getElementById('new-agent-kind').value,
      use_markers: document.getElementById('new-use-markers').checked,
    };
    const resp = await fetch('/api/sessions', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(payload),
    });
    const data = await resp.json();
    if (!resp.ok) {
      alert(data.error || '创建失败');
      return;
    }
    dialog.close();
    newSessionForm.reset();
    await refreshSessions();
    if (data.session && data.session.id) {
      connectSession(data.session.id);
    }
  });

  btnSendEnter.addEventListener('click', async () => {
    if (!activeSessionId) return;
    await fetch(`/api/sessions/${encodeURIComponent(activeSessionId)}/input`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ text: '', send_enter: true }),
    });
  });

  btnSnapshot.addEventListener('click', async () => {
    if (!activeSessionId) return;
    const resp = await fetch(`/api/sessions/${encodeURIComponent(activeSessionId)}/snapshot`);
    const data = await resp.json();
    apiTestOutput.textContent = data.text || data.error || '';
  });

  btnDeleteSession.addEventListener('click', async () => {
    if (!activeSessionId) return;
    if (!confirm('确认销毁该 tmux 会话及其中运行的进程？')) return;
    await fetch(`/api/sessions/${encodeURIComponent(activeSessionId)}`, { method: 'DELETE' });
    if (ws) {
      ws.onclose = null;
      ws.close();
      ws = null;
    }
    activeSessionId = null;
    if (term) {
      term.dispose();
      term = null;
      fitAddon = null;
      termContainer.innerHTML = '<div class="terminal-placeholder">请选择或创建一个 tmux 终端会话</div>';
    }
    await refreshSessions();
  });

  apiTestForm.addEventListener('submit', async (e) => {
    e.preventDefault();
    const apiKey = apiKeyInput.value.trim();
    const selected = sessions.find((s) => s.id === activeSessionId);
    const model = apiModelInput.value.trim() || (selected ? selected.model_alias : 'default');
    const prompt = apiPromptInput.value.trim();
    const stream = apiStreamCheckbox.checked;

    if (!apiKey) {
      apiTestOutput.textContent = '请先填写 API Key（见服务端启动日志或 data/config.json）';
      return;
    }
    if (!prompt) return;
    localStorage.setItem('gproxy_api_key', apiKey);

    btnSendApi.disabled = true;
    apiTestOutput.textContent = '请求已发送到终端 Agent，等待响应...';
    try {
      const resp = await fetch('/v1/chat/completions', {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
          Authorization: `Bearer ${apiKey}`,
        },
        body: JSON.stringify({
          model,
          stream,
          messages: [{ role: 'user', content: prompt }],
        }),
      });
      if (!resp.ok) {
        apiTestOutput.textContent = await resp.text();
        return;
      }
      if (!stream) {
        const json = await resp.json();
        apiTestOutput.textContent = JSON.stringify(json, null, 2);
      } else {
        const reader = resp.body.getReader();
        const decoder = new TextDecoder();
        let buf = '';
        apiTestOutput.textContent = '';
        while (true) {
          const { value, done } = await reader.read();
          if (done) break;
          buf += decoder.decode(value, { stream: true });
          apiTestOutput.textContent = buf;
        }
      }
    } catch (err) {
      apiTestOutput.textContent = String(err);
    } finally {
      btnSendApi.disabled = false;
      refreshSessions();
    }
  });

  function escapeHtml(str) {
    return String(str)
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;');
  }

  refreshInfo();
  refreshSessions();
  setInterval(refreshSessions, 4000);
})();
