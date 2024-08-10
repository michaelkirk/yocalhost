# Changelog

## 0.5.0 - released 2024.08.09

- BREAKING: bandwidth is now u64, not usize
- Update hyper to v1.0 - this isn't a change to the public API, but it was a
  large change so has a higher-than-likely chance of introducing a regression.

## 0.4.0 - released 2023.11.16

- Expose getters for `ThrottledServer`'s `port` and `web_root`.

## 0.3.0 - released 2023.09.14

- Optional logging of request statistics (enabled by default).

## 0.2.0 - released 2023.09.06

- Rate limits are enforced across concurrent requests.

## 0.1.0 - released 2023.09.04

Initial release

