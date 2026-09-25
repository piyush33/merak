const PAYMENT_API = "https://payments.example.com";

export class PaymentService {
  async refund(paymentId: string): Promise<void> {
    await fetch(`${PAYMENT_API}/refunds`, {
      method: "POST",
      body: JSON.stringify({ paymentId }),
    });
  }
}
