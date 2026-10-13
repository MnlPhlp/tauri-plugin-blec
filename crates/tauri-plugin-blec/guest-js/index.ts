import { Channel, invoke } from "@tauri-apps/api/core";

export type BleDevice = {
  address: string;
  name: string;
  rssi: number;
  isConnected: boolean;
  isBonded: boolean;
  services: string[];
  manufacturerData: Record<number, number[]>;
  serviceData: Record<string, number[]>;
  txPowerLevel?: number;
};

export type BleCharacteristic = {
  uuid: string;
  descriptors: string[];
  properties: number;
};

export type BleService = {
  uuid: string;
  characteristics: BleCharacteristic[];
};

export type AdapterState = "Unknown" | "On" | "Off";

export type WriteType = "withResponse" | "withoutResponse";

/**
 * A device connected or disconnected, see `getConnectionEvents()`
 */
export type ConnectionUpdate = {
  address: string;
  connected: boolean;
};

/**
 * Get the current state of the BLE adapter (on/off)
 */
export async function getAdapterState(): Promise<AdapterState> {
  let state = await invoke<AdapterState>("plugin:blec|get_adapter_state");
  return state;
}

/**
 * Scan for BLE devices
 * @param handler - A function that will be called with an array of devices found during the scan
 * @param timeout - The scan timeout in milliseconds
 */
export async function startScan(
  handler: (devices: BleDevice[]) => void,
  timeout: Number,
  allowIbeacons: boolean = false
) {
  if (!timeout) {
    timeout = 10000;
  }
  let onDevices = new Channel<BleDevice[]>();
  onDevices.onmessage = handler;
  await invoke<BleDevice[]>("plugin:blec|scan", {
    timeout,
    onDevices,
    allowIbeacons,
  });
}

/**
 * Stop scanning for BLE devices
 */
export async function stopScan() {
  await invoke("plugin:blec|stop_scan");
}

/**
 * Check if necessary permissions are granted
 * @ param askIfDenied - If true, will ask the user for permissions again, if they were denied before
 * @returns true if permissions are granted, false otherwise
 */
export async function checkPermissions(askIfDenied = true): Promise<boolean> {
  return await invoke<boolean>("plugin:blec|check_permissions", { askIfDenied });
}

/**
 * Register a handler to receive updates when the connection state changes.
 * After every connect and disconnect of any device it receives whether at
 * least one device is connected; use `getConnectionEvents()` to learn which
 * device it was.
 */
export async function getConnectionUpdates(
  handler: (connected: boolean) => void
) {
  let connection_chan = new Channel<boolean>();
  connection_chan.onmessage = handler;
  await invoke("plugin:blec|connection_state", { update: connection_chan });
}

/**
 * Register a handler that is called whenever a device connects or disconnects
 */
export async function getConnectionEvents(
  handler: (update: ConnectionUpdate) => void
) {
  let channel = new Channel<ConnectionUpdate>();
  channel.onmessage = handler;
  await invoke("plugin:blec|connection_events", { update: channel });
}

/**
 * Get all connected devices, sorted by address
 */
export async function getConnectedDevices(): Promise<BleDevice[]> {
  return await invoke<BleDevice[]>("plugin:blec|connected_devices");
}

/**
 * Register a handler to receive updates when the scanning state changes
 */
export async function getScanningUpdates(handler: (scanning: boolean) => void) {
  let scanning_chan = new Channel<boolean>();
  scanning_chan.onmessage = handler;
  await invoke("plugin:blec|scanning_state", { update: scanning_chan });
}

/**
 * Disconnect from a device
 * @param address - The device to disconnect from. Without it, the only
 * connected device is disconnected, which fails when several are connected.
 */
export async function disconnect(address?: string) {
  await invoke("plugin:blec|disconnect", { address });
}

/**
 * Disconnect from all connected devices
 */
export async function disconnectAll() {
  await invoke("plugin:blec|disconnect_all");
}

/**
 * Connect to a BLE device. Devices that are already connected stay connected.
 * @param address - The address of the device to connect to
 * @param onDisconnect - A function that will be called when the device disconnects
 * @returns A `BleConnection` to use the device with
 */
export async function connect(
  address: string,
  onDisconnect: (() => void) | null,
  allowIbeacons: boolean = false
): Promise<BleConnection> {
  let disconnectChannel = new Channel();
  if (onDisconnect) {
    disconnectChannel.onmessage = onDisconnect;
  }
  await invoke("plugin:blec|connect", {
    address: address,
    onDisconnect: disconnectChannel,
    allowIbeacons,
  });
  return new BleConnection(address);
}

/**
 * A `BleConnection` for a device that is already connected. Makes no call to
 * the backend; the methods fail if the device is not connected.
 */
export function connection(address: string): BleConnection {
  return new BleConnection(address);
}

/**
 * A connected device, as returned by `connect()`.
 *
 * Only holds the address, so it stays usable after the device reconnects.
 * The methods are the ones of the module, for this device.
 */
export class BleConnection {
  constructor(readonly address: string) {}

  /** Write a byte array to a characteristic of this device */
  async send(
    characteristic: string,
    data: number[],
    writeType: WriteType = "withResponse",
    service?: string
  ) {
    await sendTo(this.address, characteristic, data, writeType, service);
  }

  /** Write a string to a characteristic of this device */
  async sendString(
    characteristic: string,
    data: string,
    writeType: WriteType = "withResponse",
    service?: string
  ) {
    await sendStringTo(this.address, characteristic, data, writeType, service);
  }

  /** Read bytes from a characteristic of this device */
  async read(characteristic: string, service?: string): Promise<number[]> {
    return await readFrom(this.address, characteristic, service);
  }

  /** Read a string from a characteristic of this device */
  async readString(characteristic: string, service?: string): Promise<string> {
    return await readStringFrom(this.address, characteristic, service);
  }

  /** Subscribe to a characteristic of this device */
  async subscribe(
    characteristic: string,
    service: string | null,
    handler: (data: number[]) => void
  ) {
    await subscribeTo(this.address, characteristic, service, handler);
  }

  /** Subscribe to a characteristic of this device, converting the data to a string */
  async subscribeString(
    characteristic: string,
    service: string | null,
    handler: (data: string) => void
  ) {
    await subscribeStringTo(this.address, characteristic, service, handler);
  }

  /** Unsubscribe from a characteristic of this device */
  async unsubscribe(characteristic: string, service?: string) {
    await unsubscribeFrom(this.address, characteristic, service);
  }

  /** The MTU (Maximum Transfer Unit) of this device */
  async getMtu(): Promise<number> {
    return await getMtuOf(this.address);
  }

  /** The services of this device */
  async listServices(): Promise<BleService[]> {
    return await invoke<BleService[]>("plugin:blec|list_services", {
      address: this.address,
    });
  }

  /** Disconnect from this device */
  async disconnect() {
    await disconnect(this.address);
  }
}

async function sendTo(
  address: string | undefined,
  characteristic: string,
  data: number[],
  writeType: WriteType,
  service?: string
) {
  await invoke("plugin:blec|send", {
    address,
    characteristic,
    data,
    writeType,
    service,
  });
}

async function sendStringTo(
  address: string | undefined,
  characteristic: string,
  data: string,
  writeType: WriteType,
  service?: string
) {
  await invoke("plugin:blec|send_string", {
    address,
    characteristic,
    data,
    writeType,
    service,
  });
}

async function readFrom(
  address: string | undefined,
  characteristic: string,
  service?: string
): Promise<number[]> {
  return await invoke<number[]>("plugin:blec|recv", {
    address,
    characteristic,
    service,
  });
}

async function readStringFrom(
  address: string | undefined,
  characteristic: string,
  service?: string
): Promise<string> {
  return await invoke<string>("plugin:blec|recv_string", {
    address,
    characteristic,
    service,
  });
}

async function unsubscribeFrom(
  address: string | undefined,
  characteristic: string,
  service?: string
) {
  await invoke("plugin:blec|unsubscribe", {
    address,
    characteristic,
    service,
  });
}

async function subscribeTo(
  address: string | undefined,
  characteristic: string,
  service: string | null,
  handler: (data: number[]) => void
) {
  let onData = new Channel<number[]>();
  onData.onmessage = handler;
  await invoke("plugin:blec|subscribe", {
    address,
    characteristic,
    service,
    onData,
  });
}

async function subscribeStringTo(
  address: string | undefined,
  characteristic: string,
  service: string | null,
  handler: (data: string) => void
) {
  let onData = new Channel<string>();
  onData.onmessage = handler;
  await invoke("plugin:blec|subscribe_string", {
    address,
    characteristic,
    service,
    onData,
  });
}

async function getMtuOf(address: string | undefined): Promise<number> {
  return await invoke<number>("plugin:blec|mtu", { address });
}

/*
 * The functions below work on the only connected device and fail when several
 * devices are connected. Use the methods of `BleConnection` for those.
 */

/**
 * Write a byte array to a BLE characteristic of the connected device
 * @param characteristic UUID of the characteristic to write to
 * @param data Data to write to the characteristic
 */
export async function send(
  characteristic: string,
  data: number[],
  writeType: WriteType = "withResponse",
  service?: string
) {
  await sendTo(undefined, characteristic, data, writeType, service);
}

/**
 * Write a string to a BLE characteristic of the connected device
 * @param characteristic UUID of the characteristic to write to
 * @param data Data to write to the characteristic
 */
export async function sendString(
  characteristic: string,
  data: string,
  writeType: WriteType = "withResponse",
  service?: string
) {
  await sendStringTo(undefined, characteristic, data, writeType, service);
}

/**
 * Read bytes from a BLE characteristic of the connected device
 * @param characteristic UUID of the characteristic to read from
 */
export async function read(
  characteristic: string,
  service?: string
): Promise<number[]> {
  return await readFrom(undefined, characteristic, service);
}

/**
 * Read a string from a BLE characteristic of the connected device
 * @param characteristic UUID of the characteristic to read from
 */
export async function readString(
  characteristic: string,
  service?: string
): Promise<string> {
  return await readStringFrom(undefined, characteristic, service);
}

/**
 * Unsubscribe from a BLE characteristic of the connected device
 * @param characteristic UUID of the characteristic to unsubscribe from
 */
export async function unsubscribe(characteristic: string, service?: string) {
  await unsubscribeFrom(undefined, characteristic, service);
}

/**
 * Subscribe to a BLE characteristic of the connected device
 * @param characteristic UUID of the characteristic to subscribe to
 * @param handler Callback function that will be called with the data received for every notification
 */
export async function subscribe(
  characteristic: string,
  service: string | null,
  handler: (data: number[]) => void
) {
  await subscribeTo(undefined, characteristic, service, handler);
}

/**
 * Subscribe to a BLE characteristic of the connected device. Converts the received data to a string
 * @param characteristic UUID of the characteristic to subscribe to
 * @param handler Callback function that will be called with the data received for every notification
 */
export async function subscribeString(
  characteristic: string,
  service: string | null,
  handler: (data: string) => void
) {
  await subscribeStringTo(undefined, characteristic, service, handler);
}

/**
 * List device services.
 */
export async function listServices(
  address: string
): Promise<BleService[] | string> {
  let res = await invoke<string>("plugin:blec|list_services", {
    address: address,
  });
  return res;
}

/**
 * Get the MTU (Maximum Transfer Unit) of the currently connected device.
 * @returns The MTU value in bytes
 */
export async function getMtu(): Promise<number> {
  return await getMtuOf(undefined);
}

/**
 * Configure write behaviour for BLE write operations.
 * @param timeoutInMs - Timeout for write operations in milliseconds. null/undefined means no timeout.
 * @param skipWaitingOnSuccess - If true, do not wait for write completion confirmation on success.
 */
export async function setWriteBehavior(
  timeoutInMs: number | null | undefined,
  skipWaitingOnSuccess: boolean
): Promise<void> {
  await invoke("plugin:blec|set_write_behavior", {
    timeoutInMs,
    skipWaitingOnSuccess,
  });
}

/**
 * Timeouts for the GATT operations, in milliseconds.
 * Fields left out keep their current value.
 */
export type Timeouts = {
  /** Default: 20000 */
  connect?: number;
  /** Default: 15000 */
  discoverServices?: number;
  /** Default: 10000 */
  read?: number;
  /** Default: 10000 */
  write?: number;
  /** Default: 10000 */
  subscribe?: number;
  /** Default: 10000 */
  disconnect?: number;
};

/**
 * Configure the timeouts used for the GATT operations.
 * These apply to the operation as a whole, including any retries the platform
 * implementation does internally.
 * @param timeouts - The timeouts to change, in milliseconds
 */
export async function setTimeouts(timeouts: Timeouts): Promise<void> {
  await invoke("plugin:blec|set_timeouts", { timeouts });
}

/**
 * Set the MTU that will be requested when connecting on Android.
 * Other platforms negotiate the maximum MTU by default.
 * The actual MTU can be retrieved using `getMtu()` after connecting.
 * @param mtu - The MTU value to request. Use 0 to skip the MTU request.
 */
export async function setAndroidMtu(mtu: number): Promise<void> {
  await invoke("plugin:blec|set_android_mtu", { mtu });
}
