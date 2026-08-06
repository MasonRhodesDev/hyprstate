-- Profile: Dual 4K row
--@ match = Dell Left
--@ match = Dell Right

mon.row({
  { output = "DP-1", w = 3840, h = 2160, hz = 60, scale = 1.5 },
  { output = "DP-2", w = 3840, h = 2160, hz = 60, scale = 1.5 },
  { output = "eDP-2", w = 4096, h = 2560, hz = 60, scale = 1.6 },
})
