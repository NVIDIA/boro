# Hardware Monitoring Subsystem Details

## Coding style

- Code must follow guildelines in `Documentation/hwmon/submitting-patches.rst`.

- enum values in this subsystem are traditionally lowercase.
  Uppercase is permitted, but not mandatory.

## Arithmetic

- Check for overflows and underflows in arithmetc calculations

- Check for field overflows in bit field operations

## API

- New drivers must use `hwmon_device_register_with_info()` or
  `devm_hwmon_device_register_with_info()` to register with the
  hardware monitoring subsystem.

- The hardware monitoring subsystem core serializes thermal subsystem and
  sysfs operations for attributes registered with the `info` parameter of
  `hwmon_device_register_with_info()` and
  `devm_hwmon_device_register_with_info()`.
  Drivers must implement locking required for interrupt handling and for
  attributes registered by any other means. Drivers should use `hwmon_lock()`
  and `hwmon_unlock()` for this purpose.
