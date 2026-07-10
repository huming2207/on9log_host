export interface StatusResponse {
  ok: boolean;
  port: string;
  baud: number;
  uptime_ms: number;
  websocket_clients: number;
}

export type SerialLine = 'dtr' | 'rts';

const configuredBackend = import.meta.env.VITE_ON9LOG_BACKEND_URL as string | undefined;

export function apiUrl(path: string): string {
  if (!configuredBackend) {
    return path;
  }
  return new URL(path, configuredBackend).toString();
}

export function logSocketUrl(): string {
  if (configuredBackend) {
    const url = new URL('/ws/logs', configuredBackend);
    url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
    return url.toString();
  }

  const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
  return `${protocol}//${window.location.host}/ws/logs`;
}

export async function fetchStatus(signal?: AbortSignal): Promise<StatusResponse> {
  const response = await fetch(apiUrl('/api/status'), { signal });
  return parseJson<StatusResponse>(response);
}

export async function resetTarget(): Promise<void> {
  const response = await fetch(apiUrl('/api/target/reset'), {
    method: 'POST'
  });
  await parseJson(response);
}

export async function setSerialLine(line: SerialLine, value: boolean): Promise<void> {
  const response = await fetch(apiUrl('/api/serial/lines'), {
    method: 'POST',
    headers: {
      'content-type': 'application/json'
    },
    body: JSON.stringify({ [line]: value })
  });
  await parseJson(response);
}

async function parseJson<T = unknown>(response: Response): Promise<T> {
  const body = (await response.json().catch(() => null)) as T | { error?: string } | null;
  if (!response.ok) {
    const message =
      body && typeof body === 'object' && 'error' in body && body.error
        ? body.error
        : `${response.status} ${response.statusText}`;
    throw new Error(message);
  }
  return body as T;
}
