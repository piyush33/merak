import { EventBus } from "../infra/events";
import { PaymentService } from "../services/payment.service";

export function registerRefundHandler(events: EventBus, payments: PaymentService): void {
  events.on("OrderCancelled", async (event: { orderId: string; paymentId: string | null }) => {
    if (event.paymentId) {
      await payments.refund(event.paymentId);
    }
  });
}
