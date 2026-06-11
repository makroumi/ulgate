"""
ulgate - Python SDK for the ULMEN ecosystem.

    from ulgate import Client

    client = Client("http://localhost:8080", api_key="your-key")

    # Chat
    response = client.chat("What is 2+2?")
    print(response["content"])

    # Stream
    for chunk in client.chat_stream("Explain caching"):
        print(chunk, end="", flush=True)

    # Run workflow
    result = client.run(task="review auth code")
    print(result["outputs"])

    # Sessions
    reply = client.session("s1").message("Hi, I am Mehdi")
    reply = client.session("s1").message("What is my name?")

    # Custom workflows
    client.register_workflow({
        "name": "review",
        "steps": [
            {"name": "find", "tool": "code_search", "inputs": {"query": "$task"}},
            {"name": "analyze", "agent": "Review: {{find.output}}"}
        ]
    })
    result = client.run_workflow("review", task="SQL injection")

    # Observability
    print(client.dashboard())
    print(client.metrics())
    print(client.runs())
"""

__version__ = "0.1.0"

import json
import urllib.request
import urllib.error


class Session:
    """A persistent conversation session."""

    def __init__(self, client, session_id):
        self._client = client
        self.id = session_id

    def message(self, text, system=None):
        """Send a message and get a response."""
        body = {"message": text}
        if system:
            body["system"] = system
        return self._client._post(f"/v1/sessions/{self.id}/message", body)

    def history(self):
        """Get conversation history."""
        return self._client._get(f"/v1/sessions/{self.id}")

    def __repr__(self):
        return f"Session({self.id!r})"


class Client:
    """Python client for ulgate.

    Zero dependencies beyond stdlib.

    Usage:
        client = Client("http://localhost:8080", api_key="your-key")
        result = client.chat("Hello!")
    """

    def __init__(self, base_url="http://localhost:8080", api_key=None):
        self.base_url = base_url.rstrip("/")
        self.api_key = api_key

    # === Core ===

    def health(self):
        """GET /v1/health"""
        return self._get("/v1/health")

    def tools(self):
        """GET /v1/tools"""
        return self._get("/v1/tools")

    def call_tool(self, tool, **arguments):
        """POST /v1/tools/call"""
        return self._post("/v1/tools/call", {"tool": tool, "arguments": arguments})

    # === Chat ===

    def chat(self, message, system=None):
        """POST /v1/chat - blocking chat."""
        body = {"message": message}
        if system:
            body["system"] = system
        return self._post("/v1/chat", body)

    def chat_stream(self, message, system=None):
        """POST /v1/chat/stream - yields content chunks."""
        body = {"message": message}
        if system:
            body["system"] = system
        for event in self._stream("/v1/chat/stream", body):
            if event.get("type") == "token":
                yield event.get("content", "")

    # === Workflows ===

    def run(self, task=None, **kwargs):
        """POST /v1/run - run default workflow."""
        inputs = {"task": task} if task else {}
        inputs.update(kwargs)
        return self._post("/v1/run", {"input": inputs})

    def run_stream(self, task=None, **kwargs):
        """POST /v1/run/stream - yields SSE events."""
        inputs = {"task": task} if task else {}
        inputs.update(kwargs)
        yield from self._stream("/v1/run/stream", {"input": inputs})

    def run_workflow(self, name, **kwargs):
        """POST /v1/run/:name - run a named workflow."""
        return self._post(f"/v1/run/{name}", {"input": kwargs})

    def register_workflow(self, workflow):
        """POST /v1/workflows"""
        return self._post("/v1/workflows", workflow)

    def workflows(self):
        """GET /v1/workflows"""
        return self._get("/v1/workflows")

    # === Sessions ===

    def session(self, session_id):
        """Get a session handle for persistent conversations."""
        return Session(self, session_id)

    def sessions(self):
        """GET /v1/sessions"""
        return self._get("/v1/sessions")

    # === Storage ===

    def put(self, key, value):
        """POST /v1/db/put"""
        return self._post("/v1/db/put", {"key": key, "value": value})

    def get(self, key):
        """GET /v1/db/get"""
        return self._get(f"/v1/db/get?key={key}")

    def search(self, query):
        """GET /v1/db/search"""
        return self._get(f"/v1/db/search?q={query}")

    # === Observability ===

    def dashboard(self):
        """GET /v1/dashboard"""
        return self._get("/v1/dashboard")

    def metrics(self):
        """GET /v1/metrics"""
        return self._get("/v1/metrics")

    def runs(self):
        """GET /v1/runs"""
        return self._get("/v1/runs")

    def get_run(self, run_id):
        """GET /v1/runs/:id"""
        return self._get(f"/v1/runs/{run_id}")

    def logs(self):
        """GET /v1/logs"""
        return self._get("/v1/logs")

    # === Internal ===

    def _headers(self):
        h = {"Content-Type": "application/json"}
        if self.api_key:
            h["Authorization"] = f"Bearer {self.api_key}"
        return h

    def _get(self, path):
        url = f"{self.base_url}{path}"
        req = urllib.request.Request(url, headers=self._headers())
        try:
            with urllib.request.urlopen(req, timeout=30) as resp:
                return json.loads(resp.read().decode())
        except urllib.error.HTTPError as e:
            body = e.read().decode()
            try:
                return json.loads(body)
            except json.JSONDecodeError:
                return {"error": body, "status": e.code}

    def _post(self, path, body):
        url = f"{self.base_url}{path}"
        data = json.dumps(body).encode()
        req = urllib.request.Request(url, data=data, headers=self._headers(), method="POST")
        try:
            with urllib.request.urlopen(req, timeout=60) as resp:
                return json.loads(resp.read().decode())
        except urllib.error.HTTPError as e:
            body = e.read().decode()
            try:
                return json.loads(body)
            except json.JSONDecodeError:
                return {"error": body, "status": e.code}

    def _stream(self, path, body):
        url = f"{self.base_url}{path}"
        data = json.dumps(body).encode()
        req = urllib.request.Request(url, data=data, headers=self._headers(), method="POST")
        try:
            with urllib.request.urlopen(req, timeout=120) as resp:
                for line in resp:
                    line = line.decode().strip()
                    if line.startswith("data: ") and line != "data: [DONE]":
                        try:
                            yield json.loads(line[6:])
                        except json.JSONDecodeError:
                            pass
        except urllib.error.HTTPError as e:
            yield {"type": "error", "message": e.read().decode()}

    def __repr__(self):
        return f"Client({self.base_url!r})"
