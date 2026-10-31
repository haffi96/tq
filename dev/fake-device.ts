// A stand-in for the device's own programs, for config/edge.full.example.json:
//   :6000  telemetry source: one JSON line per message, at --hz (default 20)
//   :6001  lights sink: `{"type":"message",...,"payload":{"on":true}}` lines
//   :6002  teleop sink: drive commands and safe-stops
// tq-edge connects to each. The telemetry follows the drive commands, and
// reports the lights, so widgets have something live to show.
//   bun dev/fake-device.ts [--hz 20]
// Other connections (port probes) are tolerated: they get telemetry or are
// read and ignored.

const hz = Number(process.argv[process.argv.indexOf('--hz') + 1]) || 20;
const sim = { throttle: 0, steer: 0, speed: 0, heading: 0, x: 0, y: 0, battery: 12.6, lights: false, stopped: true };
const writers = new Set<{ write(s: string): void }>();

Bun.listen({
  hostname: '127.0.0.1',
  port: 6000,
  socket: {
    open: (s) => void writers.add(s),
    close: (s) => void writers.delete(s),
    error: (s) => void writers.delete(s),
    data() {},
  },
});

// Sinks: JSON lines in.
function sink(port: number, onLine: (v: Record<string, unknown>) => void) {
  Bun.listen<{ buf: string }>({
    hostname: '127.0.0.1',
    port,
    socket: {
      open: (s) => void (s.data = { buf: '' }),
      data(s, chunk) {
        s.data.buf += chunk.toString();
        let i: number;
        while ((i = s.data.buf.indexOf('\n')) >= 0) {
          const line = s.data.buf.slice(0, i);
          s.data.buf = s.data.buf.slice(i + 1);
          try {
            onLine(JSON.parse(line));
          } catch {
            // not ours (a probe)
          }
        }
      },
    },
  });
}

sink(6001, (v) => {
  const on = (v.payload as { on?: unknown } | undefined)?.on;
  if (typeof on === 'boolean') {
    sim.lights = on;
    console.log(`lights ${on ? 'on' : 'off'}`);
  }
});

let lastCommand = 0;
sink(6002, (v) => {
  if (v.type === 'safe_stop') {
    Object.assign(sim, { throttle: 0, steer: 0, stopped: true });
    console.log(`safe_stop: ${v.reason}`);
  } else if (v.type === 'command' && v.command === 'drive') {
    const a = v.args as { throttle?: number; steer?: number };
    Object.assign(sim, { throttle: a.throttle ?? 0, steer: a.steer ?? 0, stopped: false });
    lastCommand = Date.now();
  }
});

const dt = 1 / hz;
setInterval(() => {
  // Stop by itself without commands, as a motion program must.
  if (Date.now() - lastCommand > 500) Object.assign(sim, { throttle: 0, steer: 0 });
  sim.speed += (sim.throttle * 1.5 - sim.speed) * Math.min(1, dt * 3);
  sim.heading += sim.steer * dt * 1.2;
  sim.x += Math.cos(sim.heading) * sim.speed * dt;
  sim.y += Math.sin(sim.heading) * sim.speed * dt;
  sim.battery = Math.max(10.5, sim.battery - dt * (0.0005 + Math.abs(sim.speed) * 0.002));
  const t = Date.now() / 1000;
  const msg = {
    battery_v: +(sim.battery + Math.sin(t * 3) * 0.02).toFixed(3),
    speed: +sim.speed.toFixed(3),
    imu: { roll: +(Math.sin(t) * 2).toFixed(2), pitch: +(Math.cos(t * 0.7) * 1.5).toFixed(2), yaw: +((sim.heading * 180) / Math.PI).toFixed(1) },
    pose: { x: +sim.x.toFixed(3), y: +sim.y.toFixed(3) },
    lights: { on: sim.lights },
    stopped: sim.stopped,
  };
  const line = `${JSON.stringify(msg)}\n`;
  for (const w of writers) w.write(line);
}, 1000 * dt);

console.log(`fake device: telemetry :6000 at ${hz} Hz, lights :6001, teleop :6002`);
