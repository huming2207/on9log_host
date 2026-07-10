import { useCallback, useEffect, useMemo, type MutableRefObject } from 'react';
import type { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import { useXTerm } from 'react-xtermjs';

interface LogTerminalProps {
  onReady(terminal: Terminal): void;
}

export function LogTerminal({ onReady }: LogTerminalProps) {
  const fitAddon = useMemo(() => new FitAddon(), []);
  const addons = useMemo(() => [fitAddon], [fitAddon]);
  const options = useMemo(
    () => ({
      convertEol: true,
      cursorBlink: false,
      disableStdin: true,
      fontFamily:
        '"SFMono-Regular", "Cascadia Code", "Liberation Mono", Menlo, Consolas, monospace',
      fontSize: 13,
      lineHeight: 1.12,
      scrollback: 20000,
      theme: {
        background: '#0b0f14',
        foreground: '#e5edf5',
        cursor: '#7dd3fc',
        selectionBackground: '#334155',
        black: '#0b0f14',
        red: '#ef4444',
        green: '#22c55e',
        yellow: '#f59e0b',
        blue: '#60a5fa',
        magenta: '#c084fc',
        cyan: '#22d3ee',
        white: '#e5edf5',
        brightBlack: '#64748b',
        brightRed: '#f87171',
        brightGreen: '#4ade80',
        brightYellow: '#fbbf24',
        brightBlue: '#93c5fd',
        brightMagenta: '#d8b4fe',
        brightCyan: '#67e8f9',
        brightWhite: '#ffffff'
      }
    }),
    []
  );
  const { instance, ref } = useXTerm({ addons, options });
  const terminalElementRef = ref as MutableRefObject<HTMLDivElement | null>;
  const setTerminalElement = useCallback(
    (node: HTMLDivElement | null) => {
      terminalElementRef.current = node;
    },
    [terminalElementRef]
  );

  useEffect(() => {
    if (!instance) {
      return;
    }
    fitAddon.fit();
    instance.writeln('\x1b[90mon9log web terminal ready\x1b[0m');
    onReady(instance);
  }, [fitAddon, instance, onReady]);

  useEffect(() => {
    const element = ref.current;
    if (!element) {
      return;
    }

    const observer = new ResizeObserver(() => {
      fitAddon.fit();
    });
    observer.observe(element);
    return () => observer.disconnect();
  }, [fitAddon, ref]);

  return <div ref={setTerminalElement} className="terminalHost" />;
}
