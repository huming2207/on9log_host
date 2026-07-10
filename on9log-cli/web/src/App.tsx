import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import type { Terminal } from '@xterm/xterm';
import {
  Alert,
  Box,
  Button,
  Chip,
  Divider,
  LinearProgress,
  Paper,
  Stack,
  Tooltip,
  Typography
} from '@mui/material';
import { LogTerminal } from './LogTerminal';
import { fetchStatus, logSocketUrl, resetTarget, setSerialLine, type StatusResponse } from './api';

type SocketState = 'connecting' | 'open' | 'closed' | 'error';
type PendingAction = 'reset' | 'dtr-low' | 'dtr-high' | 'rts-low' | 'rts-high' | null;

const numberFormat = new Intl.NumberFormat();

export default function App() {
  const terminalRef = useRef<Terminal | null>(null);
  const socketRef = useRef<WebSocket | null>(null);
  const reconnectTimerRef = useRef<number | null>(null);

  const [socketState, setSocketState] = useState<SocketState>('connecting');
  const [status, setStatus] = useState<StatusResponse | null>(null);
  const [statusError, setStatusError] = useState<string | null>(null);
  const [lastError, setLastError] = useState<string | null>(null);
  const [messageCount, setMessageCount] = useState(0);
  const [lastLogAt, setLastLogAt] = useState<Date | null>(null);
  const [socketVersion, setSocketVersion] = useState(0);
  const [autoReconnect, setAutoReconnect] = useState(true);
  const [pendingAction, setPendingAction] = useState<PendingAction>(null);

  const socketUrl = useMemo(() => logSocketUrl(), []);

  const writeTerminal = useCallback((message: string) => {
    const terminal = terminalRef.current;
    if (!terminal) {
      return;
    }

    if (message.includes('\n')) {
      terminal.write(message.replace(/\n/g, '\r\n'));
    } else {
      terminal.writeln(message);
    }
  }, []);

  const appendNotice = useCallback(
    (message: string, color = '90') => {
      writeTerminal(`\x1b[${color}m${message}\x1b[0m`);
    },
    [writeTerminal]
  );

  useEffect(() => {
    const controller = new AbortController();

    const refresh = () => {
      void fetchStatus(controller.signal)
        .then((next) => {
          setStatus(next);
          setStatusError(null);
        })
        .catch((error: unknown) => {
          if (!controller.signal.aborted) {
            setStatusError(error instanceof Error ? error.message : String(error));
          }
        });
    };

    refresh();
    const interval = window.setInterval(refresh, 1500);
    return () => {
      controller.abort();
      window.clearInterval(interval);
    };
  }, []);

  useEffect(() => {
    if (reconnectTimerRef.current !== null) {
      window.clearTimeout(reconnectTimerRef.current);
      reconnectTimerRef.current = null;
    }

    setSocketState('connecting');
    const socket = new WebSocket(socketUrl);
    socketRef.current = socket;

    socket.onopen = () => {
      setSocketState('open');
      setLastError(null);
      appendNotice(`connected ${socketUrl}`, '32');
    };

    socket.onmessage = (event: MessageEvent<string>) => {
      const data = String(event.data);
      writeTerminal(data);
      setMessageCount((value) => value + 1);
      setLastLogAt(new Date());
    };

    socket.onerror = () => {
      setSocketState('error');
      setLastError(`websocket error: ${socketUrl}`);
    };

    socket.onclose = () => {
      setSocketState((current) => (current === 'error' ? 'error' : 'closed'));
      if (socketRef.current === socket) {
        socketRef.current = null;
      }
      if (autoReconnect) {
        reconnectTimerRef.current = window.setTimeout(() => {
          setSocketVersion((value) => value + 1);
        }, 1200);
      }
    };

    return () => {
      if (reconnectTimerRef.current !== null) {
        window.clearTimeout(reconnectTimerRef.current);
        reconnectTimerRef.current = null;
      }
      socket.close();
    };
  }, [appendNotice, autoReconnect, socketUrl, socketVersion, writeTerminal]);

  const handleTerminalReady = useCallback((terminal: Terminal) => {
    terminalRef.current = terminal;
  }, []);

  const handleClear = () => {
    terminalRef.current?.clear();
  };

  const handleReconnect = () => {
    socketRef.current?.close();
    setSocketVersion((value) => value + 1);
  };

  const runAction = async (action: PendingAction, task: () => Promise<void>, success: string) => {
    setPendingAction(action);
    try {
      await task();
      appendNotice(success, '32');
      setLastError(null);
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      setLastError(message);
      appendNotice(message, '31');
    } finally {
      setPendingAction(null);
    }
  };

  const connectionColor =
    socketState === 'open' ? 'success' : socketState === 'connecting' ? 'warning' : 'error';
  const uptime = status ? formatDuration(status.uptime_ms) : 'unknown';
  const lastLog = lastLogAt ? lastLogAt.toLocaleTimeString() : 'none';

  return (
    <Box className="appShell">
      <Paper className="topBar" square>
        <Stack
          direction={{ xs: 'column', md: 'row' }}
          spacing={1.5}
          alignItems={{ xs: 'stretch', md: 'center' }}
          justifyContent="space-between"
        >
          <Box>
            <Typography variant="h1">on9log</Typography>
            <Typography variant="body2" color="text.secondary">
              {status
                ? `${status.port} @ ${numberFormat.format(status.baud)} baud`
                : 'waiting for host status'}
            </Typography>
          </Box>

          <Stack direction="row" spacing={1} useFlexGap flexWrap="wrap" alignItems="center">
            <Chip size="small" color={connectionColor} label={`ws ${socketState}`} />
            <Chip
              size="small"
              variant="outlined"
              label={`${status?.websocket_clients ?? 0} client(s)`}
            />
            <Chip size="small" variant="outlined" label={`uptime ${uptime}`} />
            <Chip
              size="small"
              variant="outlined"
              label={`${numberFormat.format(messageCount)} msg`}
            />
          </Stack>
        </Stack>
      </Paper>

      <Box className="contentGrid">
        <Paper className="toolRail" variant="outlined">
          <Stack spacing={1.5}>
            <Box>
              <Typography variant="h2">Target</Typography>
              <Typography variant="caption" color="text.secondary">
                ESP32 serial control
              </Typography>
            </Box>
            <Button
              color="warning"
              variant="contained"
              disabled={pendingAction !== null}
              onClick={() =>
                void runAction('reset', resetTarget, 'target reset completed; decoder state reset')
              }
            >
              {pendingAction === 'reset' ? 'Resetting...' : 'Reset Target'}
            </Button>

            <Divider />

            <Stack spacing={1}>
              <Typography variant="h2">DTR</Typography>
              <Stack direction="row" spacing={1}>
                <Button
                  fullWidth
                  variant="outlined"
                  disabled={pendingAction !== null}
                  onClick={() =>
                    void runAction('dtr-low', () => setSerialLine('dtr', false), 'DTR set low')
                  }
                >
                  Low
                </Button>
                <Button
                  fullWidth
                  variant="outlined"
                  disabled={pendingAction !== null}
                  onClick={() =>
                    void runAction('dtr-high', () => setSerialLine('dtr', true), 'DTR set high')
                  }
                >
                  High
                </Button>
              </Stack>
            </Stack>

            <Stack spacing={1}>
              <Typography variant="h2">RTS</Typography>
              <Stack direction="row" spacing={1}>
                <Button
                  fullWidth
                  variant="outlined"
                  disabled={pendingAction !== null}
                  onClick={() =>
                    void runAction('rts-low', () => setSerialLine('rts', false), 'RTS set low')
                  }
                >
                  Low
                </Button>
                <Button
                  fullWidth
                  variant="outlined"
                  disabled={pendingAction !== null}
                  onClick={() =>
                    void runAction('rts-high', () => setSerialLine('rts', true), 'RTS set high')
                  }
                >
                  High
                </Button>
              </Stack>
            </Stack>

            <Divider />

            <Stack spacing={1}>
              <Typography variant="h2">Stream</Typography>
              <Button variant="outlined" onClick={handleClear}>
                Clear Terminal
              </Button>
              <Button variant="outlined" onClick={handleReconnect}>
                Reconnect
              </Button>
              <Tooltip title="When enabled, the UI reconnects after websocket close.">
                <Button
                  color={autoReconnect ? 'secondary' : 'inherit'}
                  variant={autoReconnect ? 'contained' : 'outlined'}
                  onClick={() => setAutoReconnect((value) => !value)}
                >
                  Auto Reconnect {autoReconnect ? 'On' : 'Off'}
                </Button>
              </Tooltip>
            </Stack>
          </Stack>
        </Paper>

        <Paper className="terminalPanel" variant="outlined">
          <Stack
            className="terminalHeader"
            direction={{ xs: 'column', md: 'row' }}
            spacing={1}
            alignItems={{ xs: 'stretch', md: 'center' }}
            justifyContent="space-between"
          >
            <Stack direction="row" spacing={1} useFlexGap flexWrap="wrap">
              <Chip size="small" label={`last log ${lastLog}`} variant="outlined" />
              <Chip size="small" label={socketUrl} variant="outlined" />
            </Stack>
            {pendingAction && <LinearProgress className="actionProgress" />}
          </Stack>

          {statusError && <Alert severity="warning">Status API: {statusError}</Alert>}
          {lastError && <Alert severity="error">{lastError}</Alert>}

          <Box className="terminalFrame">
            <LogTerminal onReady={handleTerminalReady} />
          </Box>
        </Paper>
      </Box>
    </Box>
  );
}

function formatDuration(ms: number): string {
  const totalSeconds = Math.floor(ms / 1000);
  const hours = Math.floor(totalSeconds / 3600);
  const minutes = Math.floor((totalSeconds % 3600) / 60);
  const seconds = totalSeconds % 60;

  if (hours > 0) {
    return `${hours}h ${minutes}m`;
  }
  if (minutes > 0) {
    return `${minutes}m ${seconds}s`;
  }
  return `${seconds}s`;
}
