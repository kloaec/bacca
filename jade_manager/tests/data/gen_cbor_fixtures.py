# Generates the CBOR test fixtures of jade_manager with cbor2 (the library used by jadepy), in the
# shape of the messages of jadepy/jade.py (requests) and of the Jade firmware (replies).
import cbor2, hashlib
out = '/home/user/bacca/jade_manager/tests/data/'
def w(name, obj):
    open(out + name, 'wb').write(cbor2.dumps(obj))
# Reply to get_version_info, fields in the order of main/versioninfo.c (non-debug build: 15 fields).
w('version_info_reply.cbor', {'id': '1', 'result': {
    'JADE_VERSION': '1.0.36', 'JADE_OTA_MAX_CHUNK': 4096, 'JADE_CONFIG': 'BLE',
    'BOARD_TYPE': 'JADE_V2', 'JADE_FEATURES': 'SB', 'IDF_VERSION': 'v5.4.1',
    'CHIP_FEATURES': '32000000', 'EFUSEMAC': 'A1B2C3D4E5F6', 'ATTESTATION_INITIALISED': True,
    'BATTERY_STATUS': 4, 'BATTERY_MILLIVOLTS': 4081, 'BATTERY_CHARGING': False,
    'JADE_STATE': 'LOCKED', 'JADE_NETWORKS': 'MAIN', 'JADE_HAS_PIN': True}})
# Reply to auth_user asking to relay a request to the pinserver (main/process/pinclient.c
# send_http_request_reply() and process_utils.c client_data_request_reply()).
w('http_request_reply.cbor', {'id': '2', 'result': {'http_request': {
    'params': {'urls': ['https://j8d.io/get_pin',
                        'http://mrrxtq6tjpbnbm7vh5jt6mpjctn7ggyfy5wegvbeff3x7jrznqawlmid.onion/get_pin'],
               'method': 'POST', 'accept': 'json', 'data': {'data': 'AAECAwQFBgcICQ=='}},
    'on-reply': 'pin'}}})
# Error reply (cbor_rpc.c rpc_build_error_reply / jade_process_reject_message_ex).
w('error_reply.cbor', {'id': '00', 'error': {'code': -32000, 'message': 'Error completing OTA',
                                            'data': b'ERR_USERDECLINED'}})
# Log message sent by the firmware between replies.
w('log_message.cbor', {'log': b'I (12345) jade: sent ok for ota_data'})
# Requests, as built by jadepy build_request().
w('ota_request.cbor', {'method': 'ota', 'id': '3', 'params': {
    'fwsize': 1445888, 'cmpsize': 712345, 'cmphash': bytes(range(32)),
    'extended_replies': False, 'fwhash': bytes(range(32, 64))}})
w('ota_data_request.cbor', {'method': 'ota_data', 'id': '4', 'params': bytes(range(256)) * 16})
w('auth_user_request.cbor', {'method': 'auth_user', 'id': '5', 'params': {'network': 'mainnet', 'epoch': 1790000000}})
w('pin_request.cbor', {'method': 'pin', 'id': '6', 'params': {'data': 'c2VydmVyIHJlcGx5'}})
