import { createTheme } from '@mui/material/styles';

export const theme = createTheme({
  palette: {
    mode: 'dark',
    background: {
      default: '#111318',
      paper: '#181b22'
    },
    primary: {
      main: '#7dd3fc'
    },
    secondary: {
      main: '#a7f3d0'
    },
    success: {
      main: '#22c55e'
    },
    warning: {
      main: '#f59e0b'
    },
    error: {
      main: '#ef4444'
    },
    divider: 'rgba(148, 163, 184, 0.22)',
    text: {
      primary: '#f8fafc',
      secondary: '#a7b0bf'
    }
  },
  shape: {
    borderRadius: 6
  },
  typography: {
    fontFamily:
      'Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif',
    h1: {
      fontSize: '1.25rem',
      fontWeight: 700,
      letterSpacing: 0
    },
    h2: {
      fontSize: '0.95rem',
      fontWeight: 700,
      letterSpacing: 0
    },
    button: {
      textTransform: 'none',
      fontWeight: 700,
      letterSpacing: 0
    }
  },
  components: {
    MuiButton: {
      styleOverrides: {
        root: {
          minHeight: 34
        }
      }
    },
    MuiChip: {
      styleOverrides: {
        root: {
          fontWeight: 700
        }
      }
    },
    MuiPaper: {
      styleOverrides: {
        root: {
          backgroundImage: 'none'
        }
      }
    }
  }
});
