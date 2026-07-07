import { injectScript } from "./src/utils.js"

const Clarity = {
    init(projectId) {
        injectScript(projectId, 'clarity-script');
    },

    setTag(key, value) {
        window.clarity('set', key, value);
    },

    identify(customerId, customSessionId, customPageId, friendlyName) {
        window.clarity('identify', customerId, customSessionId, customPageId, friendlyName);
    },

    consent(consent = true) {
        window.clarity('consent', consent);
    },

    consentV2(consentOptions = { ad_Storage: 'granted', analytics_Storage: 'granted' }) {
        window.clarity('consentv2', consentOptions);
    },

    upgrade(reason) {
        window.clarity('upgrade', reason);
    },

    event(eventName) {
        window.clarity('event', eventName);
    },
};

export default Clarity;
